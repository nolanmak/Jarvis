//! Deftform-native types + conversion into the shared `augmentagent_store::Email`.
//!
//! The store + broker + pipeline are channel-agnostic — they consume `Email`.
//! We repurpose the fields (same pattern as the GitHub channel):
//!
//! - `message_id` ← `"deft:<submission uuid>"` (globally-unique dedup key)
//! - `thread_id`  ← `"<form_id>"` (so a reply/ack can target the form)
//! - `from`       ← `"Deft form <deft:<form_id>>"`
//! - `subject`    ← `"[deft:<command>] <form_id>"`
//! - `body`       ← the parsed command + argument / query text
//! - `date`       ← submission timestamp (RFC3339) when present
//! - `account_entity_id` ← `"deft:<workspace_id>"`
//! - `platform`   ← `"deft"`
//! - `kind`       ← `"dm"` (a C&C instruction routes like a control DM)
//!
//! ## Wire shape is `REQUIRES LIVE VALIDATION`
//!
//! The vendor docs confirm the `label`/`response`/`uuid`/`custom_key` field
//! quartet and that submissions carry a `data[]` plus a submission id + uuid,
//! but do **not** pin the exact JSON envelope for `GET /responses/{formId}`
//! or the webhook body. We therefore decode **defensively**: `#[serde(default)]`
//! everywhere, unknown fields ignored, multiple plausible key spellings
//! accepted. See `docs/deft-protocol.md` §3–§4. The TODO: capture a real
//! payload (live token or webhook.site "Send test") and tighten this.

use serde::Deserialize;

use augmentagent_store::Email;

use crate::{ACCOUNT_ENTITY_ID_PREFIX, PLATFORM};

/// One field of a submission. `custom_key` is the operator-chosen, form-unique
/// anchor we map commands on (labels can be reworded; `uuid` is opaque).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DeftField {
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub response: String,
    #[serde(default)]
    pub uuid: String,
    /// Optional operator-defined key. May be absent on forms that don't set
    /// custom keys — the mapping falls back to a label match then.
    #[serde(default)]
    pub custom_key: Option<String>,
}

/// One form submission. Decoded defensively — see module docs.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DeftSubmission {
    /// Form-unique submission id (vendor: "submission ID"). Spelling not
    /// pinned by docs; accept the common alternates.
    #[serde(default, alias = "submission_id", alias = "id")]
    pub submission_id: String,
    /// Globally-unique submission uuid — the dedup key.
    #[serde(default)]
    pub uuid: String,
    /// Owning form id (REQUIRES LIVE VALIDATION: whether the responses
    /// payload echoes the form id per submission; for the webhook path we
    /// stamp it from the receiver's route).
    #[serde(default, alias = "formId", alias = "form_id")]
    pub form_id: String,
    /// Submission timestamp if present (RFC3339-ish). Best-effort.
    #[serde(default, alias = "created_at", alias = "submitted_at")]
    pub created_at: String,
    #[serde(default)]
    pub data: Vec<DeftField>,
}

impl DeftSubmission {
    /// Look a field up by `custom_key` first (the stable contract anchor),
    /// then by a case-insensitive `label` match as a fallback.
    pub fn field(&self, key: &str) -> Option<&DeftField> {
        self.data
            .iter()
            .find(|f| f.custom_key.as_deref() == Some(key))
            .or_else(|| {
                self.data
                    .iter()
                    .find(|f| f.label.eq_ignore_ascii_case(key))
            })
    }

    /// Stable dedup id. Prefer the globally-unique `uuid`; fall back to the
    /// form-scoped submission id if a payload omits the uuid.
    pub fn dedup_id(&self) -> String {
        if !self.uuid.is_empty() {
            format!("deft:{}", self.uuid)
        } else {
            format!("deft:{}:{}", self.form_id, self.submission_id)
        }
    }
}

/// Which form fields encode the command / argument / free-text query.
/// Defaults match `docs/deft-protocol.md` §5; overridable because it is the
/// user's form (the actual keys are `REQUIRES LIVE VALIDATION`).
#[derive(Debug, Clone)]
pub struct CommandFieldMap {
    pub command_key: String,
    pub arg_key: String,
    pub query_key: String,
}

impl Default for CommandFieldMap {
    fn default() -> Self {
        Self {
            command_key: "agent_command".into(),
            arg_key: "agent_arg".into(),
            query_key: "agent_query".into(),
        }
    }
}

/// Parsed command-and-control intent. Mirrors the WhatsApp control surface's
/// `ControlCommand` so the downstream dispatch is identical across channels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeftCommand {
    Approve,
    Decline,
    Revise(String),
    /// Free-text routed to the wiki/email/web `QueryHandler`.
    Query(String),
}

/// Map a submission to a [`DeftCommand`] using `map`. The verb vocabulary is
/// the same as `parse_control_command` in the WhatsApp channel so operators
/// have one mental model across C&C surfaces.
pub fn command_from_submission(sub: &DeftSubmission, map: &CommandFieldMap) -> DeftCommand {
    let command = sub
        .field(&map.command_key)
        .map(|f| f.response.trim().to_ascii_lowercase())
        .unwrap_or_default();
    let arg = sub
        .field(&map.arg_key)
        .map(|f| f.response.trim().to_string())
        .unwrap_or_default();
    let query = sub
        .field(&map.query_key)
        .map(|f| f.response.trim().to_string())
        .unwrap_or_default();

    let verb = command.split_whitespace().next().unwrap_or("");
    match verb {
        "approve" | "ok" | "okay" | "send" | "yes" | "y" => DeftCommand::Approve,
        "decline" | "skip" | "no" | "n" | "reject" => DeftCommand::Decline,
        "revise" | "redo" | "edit" | "change" => {
            // Argument can live in the dedicated arg field, or as the tail of
            // the command field itself ("revise make it shorter").
            let tail = command
                .split_once(char::is_whitespace)
                .map(|(_, r)| r.trim().to_string())
                .unwrap_or_default();
            let feedback = if !arg.is_empty() { arg } else { tail };
            DeftCommand::Revise(feedback)
        }
        _ => {
            // No recognized verb ⇒ treat the query field (or the command
            // field text) as a free-text question.
            let q = if !query.is_empty() {
                query
            } else if !command.is_empty() {
                command
            } else {
                String::new()
            };
            DeftCommand::Query(q)
        }
    }
}

impl DeftSubmission {
    /// Convert into the store's generic `Email`. `workspace_id` is stamped
    /// into `account_entity_id` so the dispatcher routes acks back to the
    /// right Deftform workspace token.
    pub fn into_email(&self, workspace_id: &str, map: &CommandFieldMap) -> Email {
        let cmd = command_from_submission(self, map);
        let (verb, body) = match &cmd {
            DeftCommand::Approve => ("approve".to_string(), "approve".to_string()),
            DeftCommand::Decline => ("decline".to_string(), "decline".to_string()),
            DeftCommand::Revise(f) => ("revise".to_string(), format!("revise {f}")),
            DeftCommand::Query(q) => ("query".to_string(), q.clone()),
        };
        Email {
            message_id: self.dedup_id(),
            thread_id: Some(self.form_id.clone()),
            from: format!("Deft form <deft:{}>", self.form_id),
            subject: format!("[deft:{verb}] {}", self.form_id),
            body,
            date: self.created_at.clone(),
            account_entity_id: Some(format!("{ACCOUNT_ENTITY_ID_PREFIX}:{workspace_id}")),
            platform: PLATFORM.to_string(),
            kind: "dm".to_string(),
        }
    }
}

/// `account_entity_id` prefix for rows from this channel.
pub const ACCOUNT_PREFIX: &str = "deft:";

/// True iff the email row came from this channel.
pub fn is_deft_email(email: &Email) -> bool {
    email
        .account_entity_id
        .as_deref()
        .is_some_and(|a| a.starts_with(ACCOUNT_PREFIX))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sub_with(fields: &[(&str, &str, &str)]) -> DeftSubmission {
        DeftSubmission {
            submission_id: "s1".into(),
            uuid: "u-123".into(),
            form_id: "OUm6T9".into(),
            created_at: "2026-05-18T10:00:00Z".into(),
            data: fields
                .iter()
                .map(|(label, resp, key)| DeftField {
                    label: label.to_string(),
                    response: resp.to_string(),
                    uuid: format!("fuuid-{label}"),
                    custom_key: if key.is_empty() {
                        None
                    } else {
                        Some(key.to_string())
                    },
                })
                .collect(),
        }
    }

    #[test]
    fn field_lookup_prefers_custom_key_then_label() {
        let s = sub_with(&[("Command", "approve", "agent_command")]);
        assert!(s.field("agent_command").is_some());
        // label fallback when no custom_key matches
        let s2 = sub_with(&[("Command", "approve", "")]);
        assert!(s2.field("agent_command").is_none());
        assert!(s2.field("Command").is_some());
    }

    #[test]
    fn maps_approve_synonyms() {
        let m = CommandFieldMap::default();
        for v in ["approve", "OK", " send ", "yes"] {
            let s = sub_with(&[("Command", v, "agent_command")]);
            assert_eq!(command_from_submission(&s, &m), DeftCommand::Approve);
        }
    }

    #[test]
    fn maps_decline_synonyms() {
        let m = CommandFieldMap::default();
        for v in ["decline", "skip", "no", "REJECT"] {
            let s = sub_with(&[("Command", v, "agent_command")]);
            assert_eq!(command_from_submission(&s, &m), DeftCommand::Decline);
        }
    }

    #[test]
    fn revise_pulls_arg_field_then_command_tail() {
        let m = CommandFieldMap::default();
        let s = sub_with(&[
            ("Command", "revise", "agent_command"),
            ("Argument", "make it warmer", "agent_arg"),
        ]);
        assert_eq!(
            command_from_submission(&s, &m),
            DeftCommand::Revise("make it warmer".into())
        );
        // arg field absent ⇒ tail of the command text
        let s2 = sub_with(&[("Command", "revise shorter please", "agent_command")]);
        assert_eq!(
            command_from_submission(&s2, &m),
            DeftCommand::Revise("shorter please".into())
        );
    }

    #[test]
    fn unknown_verb_becomes_query() {
        let m = CommandFieldMap::default();
        let s = sub_with(&[("Question", "who is Tony Siu?", "agent_query")]);
        assert_eq!(
            command_from_submission(&s, &m),
            DeftCommand::Query("who is Tony Siu?".into())
        );
    }

    #[test]
    fn dedup_prefers_uuid() {
        let s = sub_with(&[("Command", "approve", "agent_command")]);
        assert_eq!(s.dedup_id(), "deft:u-123");
        let mut s2 = s.clone();
        s2.uuid.clear();
        assert_eq!(s2.dedup_id(), "deft:OUm6T9:s1");
    }

    #[test]
    fn into_email_stamps_workspace_and_platform() {
        let m = CommandFieldMap::default();
        let s = sub_with(&[
            ("Command", "revise", "agent_command"),
            ("Argument", "less formal", "agent_arg"),
        ]);
        let e = s.into_email("ws_abc", &m);
        assert_eq!(e.platform, "deft");
        assert_eq!(e.kind, "dm");
        assert_eq!(e.message_id, "deft:u-123");
        assert_eq!(e.thread_id.as_deref(), Some("OUm6T9"));
        assert_eq!(e.account_entity_id.as_deref(), Some("deft:ws_abc"));
        assert_eq!(e.body, "revise less formal");
        assert!(e.subject.starts_with("[deft:revise]"));
        assert!(is_deft_email(&e));
    }

    #[test]
    fn defensive_decode_tolerates_alternate_keys_and_unknowns() {
        // `id` alias for submission_id, `formId` alias, extra junk ignored.
        let json = r#"{
            "id": "sub-9",
            "uuid": "uuid-9",
            "formId": "F1",
            "submitted_at": "2026-05-18T12:00:00Z",
            "totally_unknown": {"x": 1},
            "data": [
                {"label":"Command","response":"approve","uuid":"f1","custom_key":"agent_command","extra":true}
            ]
        }"#;
        let s: DeftSubmission = serde_json::from_str(json).unwrap();
        assert_eq!(s.submission_id, "sub-9");
        assert_eq!(s.form_id, "F1");
        assert_eq!(s.created_at, "2026-05-18T12:00:00Z");
        assert_eq!(
            command_from_submission(&s, &CommandFieldMap::default()),
            DeftCommand::Approve
        );
    }
}
