//! #1290 — messages to Slack contacts: approved replies and new messages the
//! owner composed.
//!
//! **Who sends.** A contact message is always sent as the owner's own Slack
//! account, through the Composio *user* connection (`augmentagent slack
//! persist-auth`), with `as_user: true`. It is never sent through the
//! interactive app's bot token: the bot is the owner's assistant in the
//! owner's DM and control channel, and Slack does not let a bot post into a
//! DM between two people anyway. See `docs/SLACK-TRANSPORT.md`, "Contact
//! sends (#1290)", for the evidence and what is still unverified.
//!
//! **Only through an approved card.** [`approve_contact_message`] is the one
//! function that posts to a contact. It sends the stored draft of an action
//! it has just claimed (`pending → sending`, or `error → sending` for a
//! retry of a failed send), to the destination recorded when the draft or
//! compose was made. Nothing the agent says in the owner's conversation,
//! and no tool output, has a path here: the owner's answers go out through
//! the app's outbox to the owner's own conversation.
//!
//! **At most once.** The per-action send ledger (`slack_contact_sends`)
//! records every attempt. A `sent` row is never posted again; an attempt
//! whose outcome is unknown (timeout, lost response) is looked for in the
//! conversation before any resend, and if the conversation cannot be read
//! the owner is asked instead of the message being resent blind.

pub mod compose;

use async_trait::async_trait;
use augmentagent_approval_discord::{strip_assumes_for_send, ApprovalActionOutcome};
use augmentagent_store::slack_contact::{
    NewSlackContactSend, SlackContactSend, SlackConversationKind, SlackSendIdentity,
    SlackSendStart, SlackSendStatus, SlackSendTarget,
};
use augmentagent_store::{ActionStatus, ActionWithEmail, Email, Store, TriageResult};
use tracing::{error, info, warn};

use crate::delivery::markdown_to_mrkdwn;

/// `email.kind` of a message the owner composed (not a reply).
pub const COMPOSE_KIND: &str = compose::COMPOSE_KIND;

/// How far before an attempt a history lookup starts (clock skew between
/// this host and Slack).
const LOOKUP_SLACK_MS: i64 = 120_000;

/// One message to post as the owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingContactMessage {
    pub channel: String,
    pub thread_ts: Option<String>,
    /// Slack mrkdwn, already converted and neutralised.
    pub text: String,
}

/// What Slack said about a posted message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostedContactMessage {
    pub channel: String,
    pub ts: String,
    /// The user Slack attributed the message to, when the response says.
    pub user: Option<String>,
    /// Present when Slack attributed the message to an app.
    pub bot_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ContactSendError {
    /// Refused before anything was posted (Slack or Composio said no, or the
    /// request never left this host).
    #[error("{0}")]
    Rejected(String),
    /// The request may have reached Slack (timeout, lost or unreadable
    /// response, a server error after the request was sent).
    #[error("{0}")]
    Unknown(String),
}

/// The owner's Slack account, as the contact-send path sees it. The daemon
/// implements it with the Composio user connection ([`crate::SlackClient`]).
#[async_trait]
pub trait ContactSendApi: Send + Sync {
    /// The Slack user id this connection posts as. `None` (or empty) means
    /// the identity is unknown and nothing may be sent.
    fn owner_user_id(&self) -> Option<String>;

    /// Post `message` as the owner.
    async fn post_as_owner(
        &self,
        message: &OutgoingContactMessage,
    ) -> Result<PostedContactMessage, ContactSendError>;

    /// The `ts` of a message by `owner_user_id` with exactly `text` in
    /// `channel` (or in the thread under `thread_ts`) since `oldest_ts`.
    async fn find_owner_message(
        &self,
        channel: &str,
        thread_ts: Option<&str>,
        oldest_ts: &str,
        owner_user_id: &str,
        text: &str,
    ) -> Result<Option<String>, ContactSendError>;
}

/// Where a reply to an ingested message goes. A message in a thread is
/// answered in that thread; a top-level message in a channel is answered in
/// a thread under it (a reply to one person should not land in the whole
/// channel); a DM or group DM is answered top level.
pub fn ingested_reply_target(
    message_id: &str,
    team_id: &str,
    channel_id: &str,
    display_name: &str,
    message_ts: &str,
    thread_ts: Option<&str>,
) -> SlackSendTarget {
    let kind = conversation_kind(channel_id, display_name);
    let thread_ts = match (thread_ts.filter(|t| !t.is_empty()), kind) {
        (Some(t), _) => Some(t.to_string()),
        (None, SlackConversationKind::Channel) => Some(message_ts.to_string()),
        (None, _) => None,
    };
    SlackSendTarget {
        message_id: message_id.into(),
        team_id: team_id.into(),
        channel_id: channel_id.into(),
        thread_ts,
        kind,
        label: Some(display_name.to_string()).filter(|l| !l.trim().is_empty()),
    }
}

/// The kind of a subscribed conversation, from its id and the label
/// `Conversation::display_name` gave it at subscribe time.
pub fn conversation_kind(channel_id: &str, display_name: &str) -> SlackConversationKind {
    let label = display_name.trim().to_ascii_lowercase();
    if channel_id.starts_with('D') || label.starts_with("dm with") {
        SlackConversationKind::Dm
    } else if label.starts_with("group dm") || label.starts_with("mpdm-") {
        SlackConversationKind::GroupDm
    } else if channel_id.starts_with('U') || channel_id.starts_with('W') {
        SlackConversationKind::User
    } else {
        SlackConversationKind::Channel
    }
}

/// The workspace a Slack row belongs to (`slack:team:<T>`).
pub fn team_of(email: &Email) -> Option<&str> {
    email
        .account_entity_id
        .as_deref()
        .and_then(|s| s.strip_prefix("slack:team:"))
        .filter(|t| !t.is_empty())
}

/// Where the message for `email` goes: the recorded target, or for a row
/// drafted before #1290 its conversation (`thread_id`), top level.
pub fn reply_target(store: &Store, email: &Email) -> Result<SlackSendTarget, String> {
    if let Some(t) = store
        .slack_send_target(&email.message_id)
        .map_err(|e| format!("reading the send target: {e}"))?
    {
        return Ok(t);
    }
    let channel = email
        .thread_id
        .clone()
        .filter(|c| !c.trim().is_empty())
        .ok_or_else(|| "no Slack conversation recorded for this draft; cannot send".to_string())?;
    let team =
        match team_of(email) {
            Some(t) => t.to_string(),
            None => {
                let workspaces = store
                    .list_active_slack_workspaces()
                    .map_err(|e| format!("reading Slack workspaces: {e}"))?;
                match workspaces.as_slice() {
                    [only] => only.team_id.clone(),
                    _ => return Err(
                        "this draft does not say which Slack workspace it belongs to; cannot send"
                            .into(),
                    ),
                }
            }
        };
    let kind = conversation_kind(&channel, "");
    Ok(SlackSendTarget {
        message_id: email.message_id.clone(),
        team_id: team,
        channel_id: channel,
        thread_ts: None,
        kind,
        label: None,
    })
}

/// The destination as the card shows it: `#general · in thread`.
pub fn describe_destination(target: &SlackSendTarget) -> String {
    let label = target
        .label
        .clone()
        .unwrap_or_else(|| target.channel_id.clone());
    let base = match target.kind {
        SlackConversationKind::Channel => label,
        SlackConversationKind::Dm => {
            if label.to_ascii_lowercase().starts_with("dm") {
                label
            } else {
                format!("DM · {label}")
            }
        }
        SlackConversationKind::GroupDm => {
            if label.to_ascii_lowercase().starts_with("group dm") {
                label
            } else {
                format!("group DM · {label}")
            }
        }
        SlackConversationKind::User => format!("DM with {label}"),
    };
    match &target.thread_ts {
        Some(ts) => format!("{base} · in thread {ts}"),
        None => base,
    }
}

/// Who the card says will send it. `None` for rows that are not Slack
/// contact messages.
pub fn sends_as_line(store: &Store, email: &Email) -> Option<String> {
    if email.platform != crate::PLATFORM {
        return None;
    }
    let team = reply_target(store, email).ok().map(|t| t.team_id)?;
    let user = store
        .get_slack_workspace_by_team(&team)
        .ok()
        .flatten()
        .map(|w| w.user_id)
        .filter(|u| !u.trim().is_empty());
    Some(match user {
        Some(u) => format!(
            "you — your Slack account `{u}` through the Composio user connection, never the app bot"
        ),
        None => format!(
            "⚠️ unknown — no Composio Slack connection with a user id for `{team}`, so Approve \
             will refuse to send (run `augmentagent slack persist-auth`)"
        ),
    })
}

fn failed(message: impl Into<String>) -> ApprovalActionOutcome {
    ApprovalActionOutcome::Failed {
        message: message.into(),
    }
}

fn resolved(store: &Store, action_id: &str) -> ApprovalActionOutcome {
    match store.get_action_with_email(action_id).ok().flatten() {
        Some(a) => ApprovalActionOutcome::AlreadyResolved {
            status: a.action.status,
            detail: a.action.error_message,
        },
        None => ApprovalActionOutcome::AlreadyResolved {
            status: "resolved".into(),
            detail: None,
        },
    }
}

/// `1700000000123` → `1700000000.123000`.
fn slack_ts(ms: i64) -> String {
    let ms = ms.max(0);
    format!("{}.{:06}", ms / 1000, (ms % 1000) * 1000)
}

fn plain_destination(row: &SlackContactSend, target: &SlackSendTarget) -> String {
    let mut t = target.clone();
    t.channel_id = row.channel_id.clone();
    t.thread_ts = row.thread_ts.clone();
    describe_destination(&t)
}

/// Send the draft of an approved Slack contact action, exactly once.
///
/// `action` is the row as loaded; `source` is the surface that decided
/// (`slack`, `discord`), recorded as the action's `status_source`. Every
/// refusal before the claim leaves the action as it was (a pending card
/// stays pending).
pub async fn approve_contact_message(
    store: &Store,
    api: Option<&dyn ContactSendApi>,
    action: &ActionWithEmail,
    source: &str,
) -> ApprovalActionOutcome {
    let action_id = action.action.id.as_str();
    // Which transition claims it: a pending card, or the retry of a send
    // that failed (the ledger says so). Anything else is decided already.
    let previous = match store.slack_contact_send(action_id) {
        Ok(p) => p,
        Err(e) => return failed(format!("reading the send ledger: {e}")),
    };
    let from = match action.action.status.as_str() {
        "pending" => ActionStatus::Pending,
        "error"
            if previous
                .as_ref()
                .is_some_and(|p| p.status != SlackSendStatus::Sent) =>
        {
            ActionStatus::Error
        }
        _ => return resolved(store, action_id),
    };

    let target = match reply_target(store, &action.email) {
        Ok(t) => t,
        Err(e) => return failed(e),
    };
    let Some(api) = api else {
        return failed(format!(
            "Slack workspace `{}` is not connected through Composio, so there is no owner \
             account to send as; run `augmentagent slack persist-auth` and approve again",
            target.team_id
        ));
    };
    let Some(owner) = api.owner_user_id().filter(|u| !u.trim().is_empty()) else {
        return failed(
            "cannot tell which Slack account would send this, so it was not sent (a contact \
             message is never sent as the app). Re-run `augmentagent slack persist-auth` and \
             approve again",
        );
    };
    let draft = action.action.draft_body.as_deref().unwrap_or_default();
    let text = markdown_to_mrkdwn(strip_assumes_for_send(draft).trim());
    if text.trim().is_empty() {
        return failed("no draft to send");
    }

    match store.claim_action_for_send(action_id, from, source) {
        Ok(true) => {}
        Ok(false) => return resolved(store, action_id),
        Err(e) => return failed(format!("claim for send failed: {e}")),
    }

    let new = NewSlackContactSend {
        action_id,
        team_id: &target.team_id,
        channel_id: &target.channel_id,
        thread_ts: target.thread_ts.as_deref(),
        identity: SlackSendIdentity::OwnerUser,
        sender_user_id: &owner,
        body: &text,
    };
    let (previous, row) = match store.start_slack_contact_send(&new) {
        Ok(SlackSendStart::Attempt { previous, row }) => (previous, row),
        Ok(SlackSendStart::AlreadySent(row)) => {
            // Delivered by an earlier attempt whose bookkeeping did not
            // finish; settle the action without posting.
            info!(
                action_id,
                "slack contact send: already delivered; not sending again"
            );
            settle_sent(store, action, &row, source, None);
            return ApprovalActionOutcome::Approved;
        }
        Err(e) => {
            let msg = format!("recording the send attempt failed: {e}");
            let _ = store.finish_send_error(action_id, &msg, None, source);
            return failed(msg);
        }
    };
    let dest = plain_destination(&row, &target);

    // An earlier attempt may have landed: look before posting again.
    if let Some(p) = previous.as_ref().filter(|p| {
        matches!(
            p.status,
            SlackSendStatus::Unknown | SlackSendStatus::Sending
        )
    }) {
        let channel = p
            .remote_channel
            .clone()
            .unwrap_or_else(|| row.channel_id.clone());
        let lookup = if channel.starts_with('U') || channel.starts_with('W') {
            Err(ContactSendError::Unknown(
                "the DM's conversation id is not known until Slack answers".into(),
            ))
        } else {
            api.find_owner_message(
                &channel,
                row.thread_ts.as_deref(),
                &slack_ts(p.attempt_started_ms - LOOKUP_SLACK_MS),
                &owner,
                &row.body,
            )
            .await
        };
        match lookup {
            Ok(Some(ts)) => {
                info!(
                    action_id,
                    ts, "slack contact send: earlier attempt found in the conversation"
                );
                let _ =
                    store.finish_slack_contact_sent(action_id, &channel, &ts, Some(&owner), None);
                let row = store
                    .slack_contact_send(action_id)
                    .ok()
                    .flatten()
                    .unwrap_or(row);
                settle_sent(store, action, &row, source, None);
                return ApprovalActionOutcome::Approved;
            }
            Ok(None) => {
                info!(
                    action_id,
                    "slack contact send: earlier attempt did not land; sending"
                );
            }
            Err(e) => {
                let msg = format!(
                    "Not sent: I could not check whether the earlier attempt reached {dest} ({e}). \
                     Look in Slack; if it is not there, Approve again to send it."
                );
                let _ =
                    store.finish_slack_contact_failed(action_id, SlackSendStatus::Unverified, &msg);
                let _ = store.finish_send_error(action_id, &msg, None, source);
                return failed(msg);
            }
        }
    }

    let message = OutgoingContactMessage {
        channel: row.channel_id.clone(),
        thread_ts: row.thread_ts.clone(),
        text: row.body.clone(),
    };
    match api.post_as_owner(&message).await {
        Ok(posted) => {
            let _ = store.finish_slack_contact_sent(
                action_id,
                &posted.channel,
                &posted.ts,
                posted.user.as_deref(),
                posted.bot_id.as_deref(),
            );
            let row = store
                .slack_contact_send(action_id)
                .ok()
                .flatten()
                .unwrap_or(row);
            let warning = identity_warning(&owner, &posted);
            if let Some(w) = &warning {
                error!(action_id, "slack contact send: {w}");
            }
            info!(action_id, channel = %posted.channel, ts = %posted.ts, "slack contact message sent as the owner");
            settle_sent(store, action, &row, source, warning.as_deref());
            ApprovalActionOutcome::Approved
        }
        Err(ContactSendError::Rejected(e)) => {
            let msg = format!(
                "Slack refused the message to {dest} ({e}); nothing was sent. Approve again to retry."
            );
            let _ = store.finish_slack_contact_failed(action_id, SlackSendStatus::Failed, &msg);
            let _ = store.finish_send_error(action_id, &msg, None, source);
            failed(msg)
        }
        Err(ContactSendError::Unknown(e)) => {
            let msg = format!(
                "The message to {dest} may or may not have been delivered ({e}). Approve again: \
                 I check the conversation first and send only if it is not there."
            );
            let _ = store.finish_slack_contact_failed(action_id, SlackSendStatus::Unknown, &msg);
            let _ = store.finish_send_error(action_id, &msg, None, source);
            warn!(action_id, "slack contact send outcome unknown: {e}");
            failed(msg)
        }
    }
}

/// Slack said who posted it; say so if that is not the owner.
fn identity_warning(owner: &str, posted: &PostedContactMessage) -> Option<String> {
    if let Some(u) = posted.user.as_deref().filter(|u| *u != owner) {
        return Some(format!(
            "sent, but Slack attributed it to `{u}`, not your account `{owner}`"
        ));
    }
    posted.bot_id.as_deref().map(|b| {
        format!(
            "sent, but Slack marked it as posted by an app (`{b}`), not by you; check how it \
             appears in the conversation"
        )
    })
}

fn settle_sent(
    store: &Store,
    action: &ActionWithEmail,
    row: &SlackContactSend,
    source: &str,
    warning: Option<&str>,
) {
    let action_id = action.action.id.as_str();
    let channel = row
        .remote_channel
        .clone()
        .unwrap_or_else(|| row.channel_id.clone());
    if let Some(ts) = row.remote_ts.as_deref() {
        // #449 on Slack: record it as the daemon's own send, per platform.
        if let Err(e) = store.record_platform_self_sent_message(
            crate::PLATFORM,
            &format!("slack:{channel}:{ts}"),
            Some(&channel),
            action.email.account_entity_id.as_deref(),
            Some(action_id),
        ) {
            warn!(action_id, "could not record the Slack self-send: {e}");
        }
    }
    if let Err(e) = store.finish_send_sent(action_id, source) {
        warn!(action_id, "could not mark the action sent: {e}");
    }
    // Replace an earlier attempt's failure message with the identity
    // warning, or nothing. (The text as sent is in the send ledger; the
    // draft stays as the owner approved it.)
    let _ = store.with_conn(|c| {
        c.execute(
            "UPDATE actions SET errorMessage = ?2 WHERE id = ?1 AND status = 'sent'",
            augmentagent_store::rusqlite::params![action_id, warning],
        )
    });
    let _ = store.mark_email_processed(&action.email.message_id, TriageResult::Reply);
}
