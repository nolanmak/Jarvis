//! D16 (#1833) — heartbeat proactive outreach, the heartbeat's side.
//!
//! When outreach is enabled the runner is handed an [`Outreach`] (the
//! daemon's trusted-contact autonomy engine, injected by the CLI so this
//! crate depends on neither). Its [`Outreach::roster`] is the only list of
//! people the model may propose messaging: active `full`-tier trusted
//! contacts, as wiki slug + display name, never a handle. A valid
//! `"outreach"` entry in the decision is handed to [`Outreach::propose`],
//! which composes it on the contact's channel and runs it through the
//! autonomy gate. Nothing here sends anything.

use async_trait::async_trait;
use serde::Serialize;

/// At most this many outreach entries per heartbeat run (D16).
pub const MAX_OUTREACH: usize = 2;
/// A proposed message longer than this is dropped, never truncated: a
/// half-message to a person is worse than none.
pub const MAX_OUTREACH_CHARS: usize = 1000;
/// The `why` line is for the owner's notice; longer than this is dropped.
pub const MAX_WHY_CHARS: usize = 300;

/// One person the model may propose messaging.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RosterEntry {
    /// The wiki slug the decision must name.
    pub slug: String,
    /// The person page's title (the slug when there is none).
    pub display_name: String,
}

/// One valid entry of the decision's `outreach` list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OutreachEntry {
    pub contact: String,
    pub message: String,
    pub why: String,
}

/// What became of one entry (for the run report and logs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OutreachResult {
    pub contact: String,
    /// `auto-sent`, `needs-approval`, `refused`, `dropped`, `failed`, or
    /// on a dry run `would-auto-send` / `would-need-approval`.
    pub status: String,
    /// The gate's reason code when it held the message back.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl OutreachResult {
    pub fn dropped(contact: Option<&str>, why: impl Into<String>) -> Self {
        Self {
            contact: contact.unwrap_or("-").to_string(),
            status: "dropped".into(),
            reason: None,
            detail: Some(why.into()),
        }
    }
}

/// The daemon side of outreach: who may be messaged, and what to do with a
/// proposal. Implemented by the autonomy crate.
#[async_trait]
pub trait Outreach: Send + Sync {
    /// Active `full`-tier trusted contacts (slug + display name only).
    fn roster(&self, now_ms: i64) -> Vec<RosterEntry>;
    /// Compose `entry` and run it through the autonomy gate. With
    /// `dry_run` nothing is stored, composed, sent or posted.
    async fn propose(&self, entry: &OutreachEntry, dry_run: bool, now_ms: i64) -> OutreachResult;
}

/// The prompt section listing the roster. Empty when the roster is.
pub fn prompt_section(roster: &[RosterEntry]) -> String {
    if roster.is_empty() {
        return String::new();
    }
    let mut out = String::from("\n## People you may message (outreach)\n");
    for person in roster {
        // Names come from wiki page titles: keep them on one line.
        let name: String = person
            .display_name
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if name.is_empty() || name == person.slug {
            out.push_str(&format!("- {}\n", person.slug));
        } else {
            out.push_str(&format!("- {} — {name}\n", person.slug));
        }
    }
    out
}
