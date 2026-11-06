//! #1833 C8 (D10) — what the approval surfaces need for trusted-contact
//! autonomy: the notice posted after a plan ran without a click (with Undo,
//! Pause this person and Pause all), the "Trusted contacts changed" notice
//! (with one Confirm per newly pending contact), and the routing of those
//! buttons to the owner-side controls the CLI installs.
//!
//! The buttons use their own `au:` custom-id namespace, like owner alerts
//! (`oa:`), so they never collide with the per-action card verbs.

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use augmentagent_store::autonomy::{PlanRow, StepRow, StepStatus};
use serenity::all::{ButtonStyle, CreateActionRow, CreateButton, CreateMessage};

/// Step kind of the reversible calendar PATCH (`calendar.add_guest`).
pub const STEP_ADD_GUEST: &str = "calendar.add_guest";
/// Step kind of an outbound message (template or drafter text).
pub const STEP_SEND: &str = "send";

/// Custom-id prefix of every autonomy control button.
pub const CUSTOM_ID_PREFIX: &str = "au:";

/// A plan that reached a terminal status, ready to show the owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutonomyNotice {
    pub plan: PlanRow,
    pub steps: Vec<StepRow>,
    /// The contact's display name (wiki slug when nothing better is known).
    pub contact_display: String,
    /// `iMessage` / `email`.
    pub channel: String,
    /// `"<title> — <local time>"` for calendar plans.
    pub event: Option<String>,
    /// An extra line, e.g. the crash-recovery "may already have been sent".
    pub note: Option<String>,
}

/// The text a step carried, when it was a send (`payload_json.text`).
pub fn step_text(step: &StepRow) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(&step.payload_json)
        .ok()?
        .get("text")?
        .as_str()
        .map(str::to_string)
}

impl AutonomyNotice {
    /// D10: Undo is offered only while a reversible step is `executed`.
    pub fn undo_available(&self) -> bool {
        self.steps
            .iter()
            .any(|s| s.kind == STEP_ADD_GUEST && s.status == StepStatus::Executed)
    }

    /// The notice body: who, what ran, each step's status and the exact text
    /// sent.
    pub fn text(&self) -> String {
        let mut out = format!(
            "🤖 Acted for {} ({}) without a click — plan {} is **{}**.",
            self.contact_display, self.channel, self.plan.id, self.plan.status
        );
        if let Some(event) = &self.event {
            out.push_str(&format!("\nEvent: {event}"));
        }
        if let Some(note) = &self.note {
            out.push_str(&format!("\n⚠ {note}"));
        }
        for step in &self.steps {
            let what = match step.kind.as_str() {
                STEP_ADD_GUEST => {
                    let guest = serde_json::from_str::<serde_json::Value>(&step.payload_json)
                        .ok()
                        .and_then(|v| v.get("calendar_email")?.as_str().map(str::to_string))
                        .unwrap_or_else(|| "the contact".into());
                    format!("add {guest} as a guest")
                }
                STEP_SEND => format!("text {}", step.target_handle),
                other => other.to_string(),
            };
            out.push_str(&format!("\n{}. {what} — {}", step.seq, step.status));
            if let Some(text) = step_text(step) {
                out.push_str(&format!("\n> {}", text.replace('\n', "\n> ")));
            }
        }
        out
    }
}

/// The "Trusted contacts changed" notice: config problems and contacts that
/// stay pending until the owner confirms them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrustNotice {
    /// `(slug, entry_hash)` of each newly pending contact.
    pub newly_pending: Vec<(String, String)>,
    pub alerts: Vec<String>,
}

impl TrustNotice {
    pub fn is_empty(&self) -> bool {
        self.newly_pending.is_empty() && self.alerts.is_empty()
    }

    pub fn text(&self) -> String {
        let mut out = String::from("🔐 Trusted contacts changed.");
        for (slug, _) in &self.newly_pending {
            out.push_str(&format!(
                "\n• `{slug}` is pending: it is not trusted until you press Confirm {slug} (or run `augmentagent trust confirm {slug}`)."
            ));
        }
        for alert in &self.alerts {
            out.push_str(&format!("\n⚠ {alert}"));
        }
        out
    }
}

/// A parsed autonomy button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Control {
    Undo { plan_id: i64 },
    Pause { slug: String },
    PauseAll,
    /// Confirm the entry whose hash starts with `entry_hash_prefix`, so a
    /// button shown for one entry can never confirm a later edit.
    Confirm { slug: String, entry_hash_prefix: String },
}

impl Control {
    pub fn custom_id(&self) -> String {
        match self {
            Control::Undo { plan_id } => format!("{CUSTOM_ID_PREFIX}undo:{plan_id}"),
            Control::Pause { slug } => format!("{CUSTOM_ID_PREFIX}pause:{slug}"),
            Control::PauseAll => format!("{CUSTOM_ID_PREFIX}pauseall:-"),
            Control::Confirm {
                slug,
                entry_hash_prefix,
            } => format!("{CUSTOM_ID_PREFIX}confirm:{slug}@{entry_hash_prefix}"),
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        let rest = raw.strip_prefix(CUSTOM_ID_PREFIX)?;
        let (verb, arg) = rest.split_once(':')?;
        if arg.is_empty() || arg.contains(':') {
            return None;
        }
        Some(match verb {
            "undo" => Control::Undo {
                plan_id: arg.parse().ok()?,
            },
            "pause" => Control::Pause {
                slug: arg.to_string(),
            },
            "pauseall" => Control::PauseAll,
            "confirm" => {
                let (slug, hash) = arg.rsplit_once('@')?;
                if slug.is_empty() || hash.len() < 8 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return None;
                }
                Control::Confirm {
                    slug: slug.to_string(),
                    entry_hash_prefix: hash.to_string(),
                }
            }
            _ => return None,
        })
    }
}

/// Owner-side effects of the buttons. The CLI implements it over the store
/// and the calendar client; every method is idempotent and returns the
/// text shown to the owner.
#[async_trait]
pub trait AutonomyControl: Send + Sync {
    async fn undo(&self, plan_id: i64) -> String;
    async fn pause(&self, slug: &str) -> String;
    async fn pause_all(&self) -> String;
    async fn confirm(&self, slug: &str, entry_hash_prefix: &str) -> String;
}

static CONTROL: OnceLock<Arc<dyn AutonomyControl>> = OnceLock::new();

/// Install the process's autonomy controls (once; later calls are ignored).
pub fn set_autonomy_control(control: Arc<dyn AutonomyControl>) -> bool {
    CONTROL.set(control).is_ok()
}

/// Run the control a button names. Unknown or unwired buttons say so.
pub async fn handle_control(custom_id: &str) -> String {
    let Some(control) = Control::parse(custom_id) else {
        return "Unrecognised autonomy control.".into();
    };
    let Some(handler) = CONTROL.get() else {
        return "Autonomy controls are not running in this process.".into();
    };
    match control {
        Control::Undo { plan_id } => handler.undo(plan_id).await,
        Control::Pause { slug } => handler.pause(&slug).await,
        Control::PauseAll => handler.pause_all().await,
        Control::Confirm {
            slug,
            entry_hash_prefix,
        } => handler.confirm(&slug, &entry_hash_prefix).await,
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The buttons a notice carries: Undo (only when reversible + executed),
/// Pause this person, Pause all.
pub fn autonomy_notice_buttons(notice: &AutonomyNotice) -> Vec<CreateButton> {
    let mut buttons = Vec::new();
    if notice.undo_available() {
        buttons.push(
            CreateButton::new(Control::Undo { plan_id: notice.plan.id }.custom_id())
                .label("Undo")
                .style(ButtonStyle::Danger),
        );
    }
    buttons.push(
        CreateButton::new(
            Control::Pause {
                slug: notice.plan.contact_slug.clone(),
            }
            .custom_id(),
        )
        .label("Pause this person")
        .style(ButtonStyle::Secondary),
    );
    buttons.push(
        CreateButton::new(Control::PauseAll.custom_id())
            .label("Pause all")
            .style(ButtonStyle::Secondary),
    );
    buttons
}

pub fn autonomy_notice_message(notice: &AutonomyNotice) -> CreateMessage {
    CreateMessage::new()
        .content(truncate(&notice.text(), 1900))
        .components(vec![CreateActionRow::Buttons(autonomy_notice_buttons(notice))])
}

/// One Confirm button per newly pending contact (Discord allows five per
/// row and five rows; anything past 25 is confirmed from the CLI).
pub fn trust_notice_message(notice: &TrustNotice) -> CreateMessage {
    let buttons: Vec<CreateButton> = notice
        .newly_pending
        .iter()
        .take(25)
        .map(|(slug, hash)| {
            CreateButton::new(
                Control::Confirm {
                    slug: slug.clone(),
                    entry_hash_prefix: hash.chars().take(16).collect(),
                }
                .custom_id(),
            )
            .label(truncate(&format!("Confirm {slug}"), 80))
            .style(ButtonStyle::Success)
        })
        .collect();
    let rows: Vec<CreateActionRow> = buttons
        .chunks(5)
        .map(|c| CreateActionRow::Buttons(c.to_vec()))
        .collect();
    CreateMessage::new()
        .content(truncate(&notice.text(), 1900))
        .components(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use augmentagent_store::autonomy::{NoticeStatus, PlanStatus};

    fn plan() -> PlanRow {
        PlanRow {
            id: 7,
            idem_key: "k".into(),
            contact_slug: "jane-doe".into(),
            tier: "actions".into(),
            inbound_message_id: Some("imessage:+12025550100:3".into()),
            thread_key: "imessage:+12025550100".into(),
            action_id: "a-1".into(),
            plan_json: "{}".into(),
            status: PlanStatus::Executed,
            notice_status: NoticeStatus::Pending,
            notice_message_id: None,
            created_at: 1,
            updated_at: 1,
        }
    }

    fn step(seq: i64, kind: &str, status: StepStatus, payload: serde_json::Value) -> StepRow {
        StepRow {
            id: seq,
            plan_id: 7,
            seq,
            kind: kind.into(),
            target_handle: "imessage:+12025550100".into(),
            account: None,
            event_ref: None,
            payload_json: payload.to_string(),
            status,
            undo_json: None,
            created_at: 1,
            updated_at: 1,
        }
    }

    fn notice(add_status: StepStatus) -> AutonomyNotice {
        AutonomyNotice {
            plan: plan(),
            steps: vec![
                step(1, STEP_ADD_GUEST, add_status, serde_json::json!({"calendar_email":"jane@work.example.test"})),
                step(2, STEP_SEND, StepStatus::Executed, serde_json::json!({"text":"Added you to Design review (Tue 3 Nov, 1:00 PM) — invite sent to jane@work.example.test. Join: https://meet.example.test/abc"})), // pii-ok synthetic
            ],
            contact_display: "jane-doe".into(),
            channel: "iMessage".into(),
            event: Some("Design review — Tue 3 Nov, 1:00 PM".into()),
            note: None,
        }
    }

    fn custom_ids(message: &CreateMessage) -> Vec<String> {
        let v = serde_json::to_value(message).unwrap();
        v["components"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|row| row["components"].as_array().cloned().unwrap_or_default())
            .map(|b| b["custom_id"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    /// C8 exit: the notice carries the exact text that went out.
    #[test]
    fn notice_contains_exact_sent_text() {
        let n = notice(StepStatus::Executed);
        let v = serde_json::to_value(autonomy_notice_message(&n)).unwrap();
        let content = v["content"].as_str().unwrap();
        assert!(content.contains("> Added you to Design review (Tue 3 Nov, 1:00 PM) — invite sent to jane@work.example.test. Join: https://meet.example.test/abc")); // pii-ok synthetic
        assert!(content.contains("jane-doe"));
        assert!(content.contains("Design review — Tue 3 Nov, 1:00 PM"));
        assert!(content.contains("1. add jane@work.example.test as a guest — executed"));
        assert!(content.contains("2. text imessage:+12025550100 — executed"));
    }

    /// C8 exit: Undo only while the reversible step is executed.
    #[test]
    fn undo_button_only_when_reversible_executed() {
        let with = custom_ids(&autonomy_notice_message(&notice(StepStatus::Executed)));
        assert_eq!(with, vec!["au:undo:7", "au:pause:jane-doe", "au:pauseall:-"]);
        for status in [StepStatus::Noop, StepStatus::Undone, StepStatus::Unknown, StepStatus::Failed] {
            let ids = custom_ids(&autonomy_notice_message(&notice(status)));
            assert_eq!(ids, vec!["au:pause:jane-doe", "au:pauseall:-"], "{status}");
        }
        // A send-only plan has nothing to undo.
        let mut send_only = notice(StepStatus::Executed);
        send_only.steps.remove(0);
        assert!(!send_only.undo_available());
    }

    #[test]
    fn controls_roundtrip_and_reject_junk() {
        for c in [
            Control::Undo { plan_id: 42 },
            Control::Pause { slug: "jane-doe".into() },
            Control::PauseAll,
            Control::Confirm { slug: "jane-doe".into(), entry_hash_prefix: "0123456789abcdef".into() },
        ] {
            assert_eq!(Control::parse(&c.custom_id()), Some(c.clone()));
            assert!(c.custom_id().len() < 100);
        }
        for junk in ["aa:x:approve", "au:undo:x", "au:confirm:jane", "au:confirm:jane@zz", "au:pause:", "au:pause:a:b", "au:explode:1"] {
            assert_eq!(Control::parse(junk), None, "{junk}");
        }
    }

    /// C7 exit: a held-back card for an active trusted contact is the
    /// normal card plus one `Not auto-sent: <reason>` line.
    #[test]
    fn trusted_card_shows_note() {
        let email = augmentagent_store::Email {
            attachments: Vec::new(),
            to: String::new(),
            cc: String::new(),
            message_id: "imessage:+12025550100:3".into(),
            thread_id: Some("imessage:+12025550100".into()),
            from: "+12025550100".into(),
            subject: "iMessage: Jane".into(),
            body: "not seeing an invite".into(),
            date: "2026-11-03T10:00:00-05:00".into(),
            account_entity_id: Some("imessage".into()),
            platform: "imessage".into(),
            kind: "dm".into(),
        };
        let plain = serde_json::to_value(crate::layout::approval_message("a-1", &email, "on it", 0)).unwrap();
        let noted = serde_json::to_value(crate::layout::approval_message_noted(
            "a-1", &email, "on it", 0, "evidence incomplete",
        ))
        .unwrap();
        assert_eq!(noted["content"], "Not auto-sent: evidence incomplete");
        let mut without_note = noted.clone();
        without_note.as_object_mut().unwrap().remove("content");
        assert_eq!(without_note, plain);
    }

    #[test]
    fn trust_notice_has_one_confirm_per_pending_contact() {
        let n = TrustNotice {
            newly_pending: vec![("jane-doe".into(), "0123456789abcdef0123".into()), ("sam-roe".into(), "fedcba9876543210".into())],
            alerts: vec!["contact #3: missing tier".into()],
        };
        let ids = custom_ids(&trust_notice_message(&n));
        assert_eq!(ids, vec!["au:confirm:jane-doe@0123456789abcdef", "au:confirm:sam-roe@fedcba9876543210"]);
        assert!(n.text().contains("missing tier"));
    }
}
