//! Block Kit for approval cards, the revise / missing-info modals and the
//! recovery reply. Pure: everything here turns data into JSON.
//!
//! Every control carries the action and the draft it was drawn for in its
//! `block_id` (`aa|<action_id>|<draft digest>`), so a click names exactly
//! one action *and* the draft the owner saw. Text commands with the same
//! short reference (`approve 3f2a9c1b`) are printed on every card, so the
//! workflow still works where controls do not render.
//!
//! Slack limits respected (Block Kit reference, read 2026-09-29): section
//! text ≤ 3000 characters, button/option text ≤ 75, `block_id` and
//! `action_id` ≤ 255, ≤ 100 select options, modal title ≤ 24, input label
//! ≤ 2000, `private_metadata` ≤ 3000.

use augmentagent_approval_discord::{
    split_assumes, split_needs_input, NeedsInput, MAX_REDRAFT_ITERATIONS, PRESETS,
};
use augmentagent_store::Email;
use serde_json::{json, Value};

use crate::delivery::mrkdwn::escape;

pub const APPROVE: &str = "aa_approve";
pub const REVISE: &str = "aa_revise";
pub const SKIP: &str = "aa_skip";
pub const FILL: &str = "aa_fill";
pub const REFINE: &str = "aa_refine";
pub const RECOMPOSE: &str = "aa_recompose";
pub const REVISE_MODAL: &str = "aa_revise_modal";
pub const FILL_MODAL: &str = "aa_fill_modal";
// #1291 — scheduled sends.
pub const SCHEDULE: &str = "aa_schedule";
pub const SCHEDULE_CONFIRM: &str = "aa_schedule_confirm";
pub const SEND_NOW: &str = "aa_send_now";
pub const RESCHEDULE: &str = "aa_reschedule";
pub const RESCHEDULE_CONFIRM: &str = "aa_reschedule_confirm";
pub const UNSCHEDULE: &str = "aa_unschedule";
pub const CANCEL_SCHEDULE: &str = "aa_cancel_schedule";
pub const SCHEDULE_MODAL: &str = "aa_schedule_modal";
pub const RESCHEDULE_MODAL: &str = "aa_reschedule_modal";

/// Length of the short action reference printed on cards.
pub const SHORT_REF_LEN: usize = 8;

const SECTION_MAX: usize = 2900;

/// The action a control belongs to and the draft it was drawn for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlRef {
    pub action_id: String,
    /// Draft digest the card showed; `None` for controls that do not act on
    /// a draft (Recompose).
    pub digest: Option<String>,
}

impl ControlRef {
    pub fn block_id(&self) -> String {
        format!(
            "aa|{}|{}",
            self.action_id,
            self.digest.as_deref().unwrap_or("-")
        )
    }

    pub fn parse(block_id: &str) -> Option<Self> {
        let mut parts = block_id.splitn(3, '|');
        if parts.next()? != "aa" {
            return None;
        }
        let action_id = parts.next().filter(|s| !s.is_empty())?.to_string();
        let digest = parts
            .next()
            .filter(|s| !s.is_empty() && *s != "-")
            .map(str::to_string);
        Some(Self { action_id, digest })
    }
}

/// FNV-1a over the stored draft body: which draft a card showed. Stable
/// across processes and releases (no `DefaultHasher`).
pub fn draft_digest(draft_body: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in draft_body.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

pub fn short_ref(action_id: &str) -> String {
    action_id.chars().take(SHORT_REF_LEN).collect()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn quote(text: &str) -> String {
    text.lines()
        .map(|l| format!("> {}", escape(l)))
        .collect::<Vec<_>>()
        .join("\n")
}

fn section(text: String) -> Value {
    json!({"type": "section", "text": {"type": "mrkdwn", "text": truncate(&text, SECTION_MAX)}})
}

fn context(text: String) -> Value {
    json!({"type": "context", "elements": [{"type": "mrkdwn", "text": truncate(&text, SECTION_MAX)}]})
}

fn button(text: &str, action_id: &str, value: &str, style: Option<&str>) -> Value {
    let mut b = json!({
        "type": "button",
        "text": {"type": "plain_text", "text": text},
        "action_id": action_id,
        "value": value,
    });
    if let Some(style) = style {
        b["style"] = json!(style);
    }
    b
}

fn platform_label(email: &Email) -> String {
    match email.platform.as_str() {
        "" => "email".into(),
        "gmail" => "Gmail".into(),
        "slack" => "Slack".into(),
        "discord" => "Discord".into(),
        "imessage" => "iMessage".into(),
        "telegram" => "Telegram".into(),
        "github" => "GitHub".into(),
        "gcal" => "Calendar".into(),
        other => other.to_string(),
    }
}

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

/// Everything a card shows. `display_draft` is the stored draft plus the
/// envelope markers the Discord card shows (`[to: …]`, …).
pub struct CardInput<'a> {
    pub action_id: &'a str,
    pub email: &'a Email,
    pub display_draft: &'a str,
    pub digest: &'a str,
    pub redraft_count: i64,
    /// A line shown above the card (the reminder header, the Revise-result
    /// header).
    pub note: Option<&'a str>,
    /// `None` while the action is pending (controls are shown); otherwise
    /// the state line that replaces them.
    pub status_line: Option<&'a str>,
    /// Recompose is offered on a superseded card with a draft worth keeping.
    pub offer_recompose: bool,
    /// #1290 — where a Slack contact message goes (`#general · in thread …`).
    pub destination: Option<&'a str>,
    /// #1290 — who a Slack contact message is sent as.
    pub sends_as: Option<&'a str>,
    /// #1290 — a Slack send that failed after approval can be retried.
    pub offer_retry: bool,
    /// #1291 — the action is an armed scheduled send; this is its fire time
    /// as shown (`Wed Sep 30, 9:00 AM EDT (America/New_York)`). The card is
    /// then drawn as the scheduled notice.
    pub scheduled_for: Option<&'a str>,
}

/// The fallback `text` and the `blocks` of a card.
pub fn card(input: &CardInput<'_>) -> (String, Value) {
    let email = input.email;
    let id = input.action_id;
    let r = short_ref(id);
    let merge = email.kind == "identity_merge";
    let (human, needs) = split_needs_input(input.display_draft);
    let (human, assumes) = split_assumes(&human);
    let at_cap = input.redraft_count >= MAX_REDRAFT_ITERATIONS;
    let subject = if email.subject.trim().is_empty() {
        "(no subject)".to_string()
    } else {
        email.subject.clone()
    };

    let mut blocks = Vec::new();
    if let Some(note) = input.note.filter(|n| !n.trim().is_empty()) {
        blocks.push(section(escape(note)));
    }
    let compose = email.kind == crate::contact::COMPOSE_KIND;
    let kind = if merge {
        "Merge proposal"
    } else if compose {
        "New message"
    } else {
        "Reply draft"
    };
    // #1291 — an armed scheduled send is drawn as the scheduled notice.
    let kind = if input.status_line.is_none() && input.scheduled_for.is_some() {
        "🗓️ Scheduled send"
    } else {
        kind
    };
    blocks.push(section(format!(
        "*{kind}* · {}\n*{}*",
        escape(&platform_label(email)),
        escape(&truncate(&subject, 250))
    )));
    blocks.push(json!({
        "type": "section",
        "fields": [
            {"type": "mrkdwn", "text": format!("*{}*\n{}", if compose { "To" } else { "From" }, escape(&truncate(&email.from, 200)))},
            {"type": "mrkdwn", "text": format!("*Ref*\n`{r}`")},
        ]
    }));
    if input.destination.is_some() || input.sends_as.is_some() {
        let mut lines = Vec::new();
        if let Some(d) = input.destination {
            lines.push(format!("*Goes to* {}", escape(&truncate(d, 300))));
        }
        if let Some(who) = input.sends_as {
            lines.push(format!("*Sends as* {}", escape(&truncate(who, 400))));
        }
        blocks.push(section(lines.join("\n")));
    }
    if !email.body.trim().is_empty() && !merge {
        blocks.push(section(format!(
            "*Their message*\n{}",
            quote(&truncate(email.body.trim(), 1200))
        )));
    }
    let draft_title = if merge {
        "*Proposal*"
    } else if compose {
        "*Message*"
    } else {
        "*Draft reply*"
    };
    let draft_text = if human.trim().is_empty() {
        "_(empty)_".to_string()
    } else {
        quote(&truncate(human.trim(), 2600))
    };
    blocks.push(section(format!("{draft_title}\n{draft_text}")));
    if !needs.is_empty() {
        let mut s = String::from("*⚠️ Needs your input*\n");
        for n in &needs {
            s.push_str(&format!(
                "• *{}* — {}\n",
                label_for(&n.kind),
                escape(n.text.trim())
            ));
        }
        s.push_str("_Use *Provide missing info* to supply these; the draft is rewritten with your values._");
        blocks.push(section(s));
    }
    if !assumes.is_empty() {
        let mut s = String::from("*⚠ Assumes*\n");
        for f in &assumes {
            s.push_str(&format!("• {}\n", escape(f)));
        }
        s.push_str("_Not verified — Revise if any of these is wrong._");
        blocks.push(section(s));
    }

    let version = if input.redraft_count == 0 {
        "draft v1".to_string()
    } else if at_cap {
        format!(
            "draft v{} · refine cap reached — Approve/Skip or use Revise",
            input.redraft_count + 1
        )
    } else {
        format!("draft v{}", input.redraft_count + 1)
    };
    let control = ControlRef {
        action_id: id.to_string(),
        digest: Some(input.digest.to_string()),
    };
    if let (None, Some(when)) = (input.status_line, input.scheduled_for) {
        return scheduled_notice(input, &subject, &version, &control, when, blocks);
    }
    match input.status_line {
        None => {
            let value = control.block_id();
            let mut elements = if merge {
                vec![
                    button("Approve & Merge", APPROVE, &value, Some("primary")),
                    button("Skip", SKIP, &value, None),
                ]
            } else {
                vec![
                    button("Approve & Send", APPROVE, &value, Some("primary")),
                    button("Revise", REVISE, &value, None),
                    button("Skip", SKIP, &value, None),
                ]
            };
            if !merge {
                elements.push(button("Schedule…", SCHEDULE, &value, None));
            }
            if !merge && !needs.is_empty() {
                elements.push(button("Provide missing info", FILL, &value, Some("danger")));
            }
            if !merge && !at_cap {
                elements.push(json!({
                    "type": "static_select",
                    "action_id": REFINE,
                    "placeholder": {"type": "plain_text", "text": "Quick refine…"},
                    "options": PRESETS.iter().map(|p| json!({
                        "text": {"type": "plain_text", "text": truncate(p.label, 75)},
                        "value": p.id,
                    })).collect::<Vec<_>>(),
                }));
            }
            blocks.push(json!({"type": "actions", "block_id": value, "elements": elements}));
            let mut commands = format!("Or reply: `approve {r}` · `skip {r}`");
            if !merge {
                commands.push_str(&format!(" · `revise {r} &lt;what to change&gt;`"));
                if !at_cap {
                    commands.push_str(&format!(" · `refine {r} shorter`"));
                }
                commands.push_str(&format!(" · `schedule {r} tomorrow 9am`"));
            }
            blocks.push(context(format!("{version} · {commands}")));
        }
        Some(line) => {
            blocks.push(section(escape(line)));
            if input.offer_retry {
                let value = control.block_id();
                blocks.push(json!({
                    "type": "actions",
                    "block_id": value,
                    "elements": [button("Retry send", APPROVE, &value, Some("primary"))],
                }));
                blocks.push(context(format!(
                    "{version} · or reply `approve {r}` to retry — a send that may have landed is looked for first"
                )));
            } else if input.offer_recompose {
                let recompose = ControlRef {
                    action_id: id.to_string(),
                    digest: None,
                };
                blocks.push(json!({
                    "type": "actions",
                    "block_id": recompose.block_id(),
                    "elements": [button("Recompose", RECOMPOSE, &recompose.block_id(), None)],
                }));
                blocks.push(context(format!("{version} · or reply `recompose {r}`")));
            } else {
                blocks.push(context(version));
            }
        }
    }
    let text = match input.status_line {
        None if compose => format!("Approval needed: {subject}"),
        None => format!("Approval needed: {} — from {}", subject, email.from),
        Some(line) => format!("{line} {subject}"),
    };
    (truncate(&text, 3000), Value::Array(blocks))
}

/// #1291 — the card of an armed scheduled send: when it goes, where, as
/// whom, and the four ways to change that (Send now, Reschedule, Back to
/// queue, Cancel), with their text commands.
fn scheduled_notice(
    input: &CardInput<'_>,
    subject: &str,
    version: &str,
    control: &ControlRef,
    when: &str,
    mut blocks: Vec<Value>,
) -> (String, Value) {
    let r = short_ref(input.action_id);
    blocks.push(section(format!("*Sends* {}", escape(when))));
    let value = control.block_id();
    blocks.push(json!({
        "type": "actions",
        "block_id": value,
        "elements": [
            button("Send now", SEND_NOW, &value, Some("primary")),
            button("Reschedule…", RESCHEDULE, &value, None),
            button("Back to queue", UNSCHEDULE, &value, None),
            button("Cancel schedule", CANCEL_SCHEDULE, &value, Some("danger")),
        ],
    }));
    blocks.push(context(format!(
        "{version} · Or reply: `sendnow {r}` · `reschedule {r} &lt;when&gt;` · `requeue {r}` · `cancel {r}`"
    )));
    let text = format!("Scheduled: {subject} — sends {when}");
    (truncate(&text, 3000), Value::Array(blocks))
}

/// #1291 — the Schedule (or Reschedule) modal: a preset or a typed time,
/// read in `zone_name`. Nothing is armed by submitting it; the owner
/// confirms the resolved time next.
pub fn schedule_modal(
    ctx: &ModalContext,
    subject: &str,
    zone_name: &str,
    now_shown: &str,
    reschedule: bool,
) -> Value {
    let (callback, title) = if reschedule {
        (RESCHEDULE_MODAL, "Reschedule send")
    } else {
        (SCHEDULE_MODAL, "Schedule send")
    };
    let options: Vec<Value> = augmentagent_approval_discord::timeparse::SCHEDULE_PRESETS
        .iter()
        .map(|(label, token)| {
            json!({"text": {"type": "plain_text", "text": truncate(label, 75)}, "value": token})
        })
        .collect();
    modal_shell(
        callback,
        title,
        "Preview",
        ctx,
        vec![
            section(format!(
                "*{}*\nTimes are in *{}* — it is {} now. You confirm the exact time next.",
                escape(&truncate(subject, 250)),
                escape(zone_name),
                escape(now_shown)
            )),
            json!({
                "type": "input",
                "block_id": "preset",
                "optional": true,
                "label": {"type": "plain_text", "text": "Pick a time"},
                "element": {"type": "static_select", "action_id": "preset",
                            "placeholder": {"type": "plain_text", "text": "Choose…"},
                            "options": options},
            }),
            json!({
                "type": "input",
                "block_id": "when",
                "optional": true,
                "label": {"type": "plain_text", "text": "…or type one"},
                "hint": {"type": "plain_text", "text": "tomorrow 9am · fri 14:30 · in 3h · 7pm · 2026-10-01 09:00"},
                "element": {"type": "plain_text_input", "action_id": "when",
                            "placeholder": {"type": "plain_text", "text": "tomorrow 9am"}},
            }),
        ],
    )
}

/// #1291 — the confirmation shown before a schedule is armed (or moved):
/// the resolved time with its zone, any daylight-saving note, where it goes,
/// and one Confirm button carrying the instant. `(text, blocks)`.
#[allow(clippy::too_many_arguments)]
pub fn schedule_confirmation(
    action_id: &str,
    digest: &str,
    at_ms: i64,
    when: &str,
    dst_note: Option<&str>,
    destination: Option<&str>,
    subject: &str,
    reschedule: bool,
) -> (String, Value) {
    let r = short_ref(action_id);
    let (ask, confirm, control) = if reschedule {
        ("Move this send to", "Confirm new time", RESCHEDULE_CONFIRM)
    } else {
        (
            "Schedule this send for",
            "Confirm schedule",
            SCHEDULE_CONFIRM,
        )
    };
    let mut body = format!(
        "*{ask}* *{}*?\n{}",
        escape(when),
        escape(&truncate(subject, 250))
    );
    if let Some(d) = destination {
        body.push_str(&format!(" · goes to {}", escape(&truncate(d, 300))));
    }
    if let Some(note) = dst_note {
        body.push_str(&format!("\n⚠️ {}", escape(note)));
    }
    let control_ref = ControlRef {
        action_id: action_id.to_string(),
        digest: Some(digest.to_string()),
    };
    let blocks = json!([
        section(body),
        {"type": "actions", "block_id": control_ref.block_id(),
         "elements": [button(confirm, control, &at_ms.to_string(), Some("primary"))]},
        context(format!("Nothing is scheduled until you confirm. Ref `{r}`.")),
    ]);
    (format!("{ask} {when}? Confirm to schedule it."), blocks)
}

/// The value of a submitted `static_select` in a `view`.
pub fn view_selected(view: &Value, block_id: &str, action_id: &str) -> Option<String> {
    view.pointer(&format!(
        "/state/values/{block_id}/{action_id}/selected_option/value"
    ))
    .and_then(Value::as_str)
    .filter(|s| !s.is_empty())
    .map(str::to_string)
}

/// A card that a newer card for the same action replaced.
pub fn replaced_card(email: &Email, action_id: &str) -> (String, Value) {
    let text = format!(
        "↪️ Reposted below — act on the newest card for *{}* (ref `{}`).",
        escape(&truncate(&email.subject, 250)),
        short_ref(action_id)
    );
    (
        "Reposted below — act on the newest card.".into(),
        Value::Array(vec![section(text)]),
    )
}

/// The one-line reply to a decision, with a Recompose button when the
/// action is a recoverable supersede.
pub fn reply_blocks(text: &str, recompose_for: Option<&str>) -> Option<Value> {
    let action_id = recompose_for?;
    let r = ControlRef {
        action_id: action_id.to_string(),
        digest: None,
    };
    Some(json!([
        section(escape(text)),
        {"type": "actions", "block_id": r.block_id(),
         "elements": [button("Recompose", RECOMPOSE, &r.block_id(), None)]},
    ]))
}

/// What a modal remembers between opening and submission.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModalContext {
    pub action_id: String,
    pub digest: String,
    /// Where the card is, so the answer goes next to it.
    pub channel: Option<String>,
    pub ts: Option<String>,
}

fn modal_shell(
    callback_id: &str,
    title: &str,
    submit: &str,
    ctx: &ModalContext,
    blocks: Vec<Value>,
) -> Value {
    json!({
        "type": "modal",
        "callback_id": callback_id,
        "title": {"type": "plain_text", "text": truncate(title, 24)},
        "submit": {"type": "plain_text", "text": truncate(submit, 24)},
        "close": {"type": "plain_text", "text": "Cancel"},
        "private_metadata": serde_json::to_string(ctx).unwrap_or_default(),
        "blocks": blocks,
    })
}

/// The Revise modal: one multi-line input for the feedback.
pub fn revise_modal(ctx: &ModalContext, subject: &str, draft: &str) -> Value {
    let (human, _) = split_needs_input(draft);
    let (human, _) = split_assumes(&human);
    modal_shell(
        REVISE_MODAL,
        "Revise draft",
        "Revise",
        ctx,
        vec![
            section(format!(
                "*{}*\n{}",
                escape(&truncate(subject, 250)),
                quote(&truncate(human.trim(), 2000))
            )),
            json!({
                "type": "input",
                "block_id": "feedback",
                "label": {"type": "plain_text", "text": "What should change?"},
                "element": {"type": "plain_text_input", "action_id": "feedback", "multiline": true},
            }),
        ],
    )
}

/// The missing-info modal: one input per unresolved ask (at most five, as
/// on Discord; the rest stay listed on the card).
pub fn fill_modal(ctx: &ModalContext, needs: &[NeedsInput]) -> Value {
    let inputs = needs
        .iter()
        .take(5)
        .enumerate()
        .map(|(i, n)| {
            json!({
                "type": "input",
                "block_id": format!("ask_{i}"),
                "label": {"type": "plain_text", "text": truncate(&format!("{} — {}", label_for(&n.kind), n.text), 2000)},
                "element": {"type": "plain_text_input", "action_id": "value", "multiline": true},
            })
        })
        .collect();
    modal_shell(
        FILL_MODAL,
        "Provide missing info",
        "Rewrite draft",
        ctx,
        inputs,
    )
}

/// The submitted text of input `block_id`/`action_id` in a `view`.
pub fn view_value(view: &Value, block_id: &str, action_id: &str) -> Option<String> {
    view.pointer(&format!("/state/values/{block_id}/{action_id}/value"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn email() -> Email {
        Email {
            attachments: Vec::new(),
            to: String::new(),
            cc: String::new(),
            message_id: "m".into(),
            thread_id: Some("C1".into()),
            from: "Contact <a&b>".into(),
            subject: "Lunch?".into(),
            body: "Free Tuesday?".into(),
            date: String::new(),
            account_entity_id: None,
            platform: "slack".into(),
            kind: "dm".into(),
        }
    }

    fn input<'a>(email: &'a Email, status: Option<&'a str>) -> CardInput<'a> {
        CardInput {
            action_id: "3f2a9c1b-0000-4000-8000-000000000001",
            email,
            display_draft: "Tuesday works.",
            digest: "abc",
            redraft_count: 0,
            note: None,
            status_line: status,
            offer_recompose: false,
            destination: None,
            sends_as: None,
            offer_retry: false,
            scheduled_for: None,
        }
    }

    fn action_ids(blocks: &Value) -> Vec<String> {
        blocks
            .as_array()
            .unwrap()
            .iter()
            .filter(|b| b["type"] == "actions")
            .flat_map(|b| b["elements"].as_array().unwrap().clone())
            .map(|e| e["action_id"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn control_refs_round_trip_and_reject_foreign_ids() {
        let r = ControlRef {
            action_id: "a-1".into(),
            digest: Some("d".into()),
        };
        assert_eq!(ControlRef::parse(&r.block_id()), Some(r));
        let none = ControlRef {
            action_id: "a-1".into(),
            digest: None,
        };
        assert_eq!(ControlRef::parse(&none.block_id()), Some(none));
        assert_eq!(ControlRef::parse("other|a|b"), None);
        assert_eq!(ControlRef::parse("aa||b"), None);
    }

    #[test]
    fn a_pending_card_has_every_control_and_the_text_commands() {
        let e = email();
        let (text, blocks) = card(&input(&e, None));
        assert!(text.contains("Lunch?"));
        assert_eq!(
            action_ids(&blocks),
            vec![APPROVE, REVISE, SKIP, SCHEDULE, REFINE]
        );
        let all = blocks.to_string();
        assert!(all.contains("approve 3f2a9c1b"), "{all}");
        assert!(all.contains("revise 3f2a9c1b"));
        assert!(all.contains("&lt;a&amp;b&gt;"), "mrkdwn is escaped: {all}");
    }

    #[test]
    fn a_decided_card_has_no_decision_controls() {
        let e = email();
        let (_, blocks) = card(&input(&e, Some("✅ Sent.")));
        assert!(action_ids(&blocks).is_empty());
        assert!(blocks.to_string().contains("✅ Sent."));
        let mut recover = input(&e, Some("♻️ A newer version replaced this draft."));
        recover.offer_recompose = true;
        let (_, blocks) = card(&recover);
        assert_eq!(action_ids(&blocks), vec![RECOMPOSE]);
    }

    #[test]
    fn at_the_refine_cap_the_preset_menu_is_gone() {
        let e = email();
        let mut at_cap = input(&e, None);
        at_cap.redraft_count = MAX_REDRAFT_ITERATIONS;
        let (_, blocks) = card(&at_cap);
        assert_eq!(action_ids(&blocks), vec![APPROVE, REVISE, SKIP, SCHEDULE]);
        assert!(blocks.to_string().contains("refine cap reached"));
    }

    #[test]
    fn digests_are_stable_and_distinguish_drafts() {
        assert_eq!(draft_digest("a"), draft_digest("a"));
        assert_ne!(draft_digest("a"), draft_digest("b"));
        assert_eq!(draft_digest("").len(), 16);
    }
}
