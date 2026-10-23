//! #1282 — Slack on the shared surface contracts from `augmentagent-store`.
//!
//! This module is the only place that knows how Slack identifiers map onto
//! the transport-neutral references. Nothing here talks to Slack, and no
//! value is ever turned into a Discord snowflake: every ID stays the opaque
//! string Slack issued.
//!
//! | Shared reference           | Slack value                                          |
//! |----------------------------|------------------------------------------------------|
//! | `SurfacePlatform`          | `slack`                                              |
//! | `SurfaceAccountRef`        | `team:<team_id>`, or `enterprise:<id>/team:<team_id>` |
//! | `SurfaceOwnerRef`          | account + the owner's user ID                        |
//! | `SurfaceConversationRef`   | channel/DM ID; `thread_id` = parent `thread_ts`      |
//! | `SurfaceMessageRef`        | `<channel_id>:<ts>` inside its conversation          |
//! | `SurfaceReplyTarget`       | conversation (thread when replying in one) + quoted `<channel_id>:<ts>` |
//!
//! Storage keys come from the shared `storage_key` helpers, which length-prefix
//! every part, so a Slack workspace, a WhatsApp device and a Discord guild
//! with look-alike IDs never share a row, and two workspaces with the same
//! channel ID stay apart.
//!
//! Enterprise Grid: the enterprise ID is part of the account identity, so the
//! same team referenced with and without it is two accounts. Callers build
//! the [`SlackWorkspace`] from the stored install record, never per event.
//!
//! Validation is deliberately narrow. Slack documents its IDs as opaque, so
//! only the character set is checked (ASCII letters and digits); prefixes such
//! as `T`, `U` or `C` are not enforced. Timestamps must be `<digits>.<digits>`,
//! the documented `ts` shape.
//!
//! The capability tables at the bottom are the Slack column of the feature
//! matrix. Each row states whether the capability is implemented on the Slack
//! surface in this repo (`Supported`), not implemented (`Unsupported`) or not
//! yet shown to be possible at all (`Unproven`), and which issue owns it.
//! `docs/slack-parity-matrix.json` (#1300, [`crate::parity`]) maps every epic
//! row onto these entries, and its check fails when the two disagree.

use augmentagent_store::{
    SurfaceAccountRef, SurfaceCapabilities, SurfaceCapability, SurfaceConversationRef,
    SurfaceMessageRef, SurfaceOwnerRef, SurfacePlatform, SurfaceRefError, SurfaceReplyTarget,
};

/// The `SurfacePlatform` value for Slack. Same string as [`crate::PLATFORM`].
pub const SLACK_SURFACE_PLATFORM: &str = "slack";

const TEAM_PREFIX: &str = "team:";
const ENTERPRISE_PREFIX: &str = "enterprise:";
const ACCOUNT_SEPARATOR: char = '/';
const MESSAGE_SEPARATOR: char = ':';

fn platform() -> SurfacePlatform {
    SurfacePlatform::new(SLACK_SURFACE_PLATFORM).expect("static platform name is valid")
}

/// Slack IDs are opaque; we only refuse strings that cannot be one.
fn slack_id<'a>(value: &'a str, name: &'static str) -> Result<&'a str, SurfaceRefError> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return Err(SurfaceRefError::Invalid(name));
    }
    Ok(value)
}

/// A Slack `ts` is `<seconds>.<fraction>`, both non-empty and all digits.
fn slack_ts<'a>(value: &'a str, name: &'static str) -> Result<&'a str, SurfaceRefError> {
    let Some((seconds, fraction)) = value.split_once('.') else {
        return Err(SurfaceRefError::Invalid(name));
    };
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
    if digits(seconds) && digits(fraction) {
        Ok(value)
    } else {
        Err(SurfaceRefError::Invalid(name))
    }
}

/// One installed Slack workspace: the account on the shared surface.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SlackWorkspace {
    team_id: String,
    enterprise_id: Option<String>,
}

impl SlackWorkspace {
    pub fn new(team_id: &str, enterprise_id: Option<&str>) -> Result<Self, SurfaceRefError> {
        slack_id(team_id, "slack team ID")?;
        if let Some(enterprise) = enterprise_id {
            slack_id(enterprise, "slack enterprise ID")?;
        }
        Ok(Self {
            team_id: team_id.to_string(),
            enterprise_id: enterprise_id.map(str::to_string),
        })
    }

    pub fn team_id(&self) -> &str {
        &self.team_id
    }

    pub fn enterprise_id(&self) -> Option<&str> {
        self.enterprise_id.as_deref()
    }

    /// `team:T…` or `enterprise:E…/team:T…`.
    pub fn account(&self) -> SurfaceAccountRef {
        let account_id = match &self.enterprise_id {
            Some(enterprise) => format!(
                "{ENTERPRISE_PREFIX}{enterprise}{ACCOUNT_SEPARATOR}{TEAM_PREFIX}{}",
                self.team_id
            ),
            None => format!("{TEAM_PREFIX}{}", self.team_id),
        };
        SurfaceAccountRef::new(platform(), account_id).expect("validated slack IDs")
    }

    /// Inverse of [`SlackWorkspace::account`]; refuses other platforms and
    /// account IDs this mapping did not produce.
    pub fn from_account(account: &SurfaceAccountRef) -> Result<Self, SurfaceRefError> {
        if account.platform().as_str() != SLACK_SURFACE_PLATFORM {
            return Err(SurfaceRefError::Invalid("slack surface platform"));
        }
        let account_id = account.account_id();
        let (enterprise, team) = match account_id.strip_prefix(ENTERPRISE_PREFIX) {
            Some(rest) => {
                let (enterprise, team) = rest
                    .split_once(ACCOUNT_SEPARATOR)
                    .ok_or(SurfaceRefError::Invalid("slack account ID"))?;
                (Some(enterprise), team)
            }
            None => (None, account_id),
        };
        let team = team
            .strip_prefix(TEAM_PREFIX)
            .ok_or(SurfaceRefError::Invalid("slack account ID"))?;
        Self::new(team, enterprise)
    }

    pub fn owner(&self, user_id: &str) -> Result<SurfaceOwnerRef, SurfaceRefError> {
        slack_id(user_id, "slack user ID")?;
        SurfaceOwnerRef::new(self.account(), user_id)
    }

    /// A channel, group or DM; with `thread_ts`, the thread under that
    /// parent message, which is a distinct conversation from the channel.
    pub fn conversation(
        &self,
        channel_id: &str,
        thread_ts: Option<&str>,
    ) -> Result<SurfaceConversationRef, SurfaceRefError> {
        slack_id(channel_id, "slack channel ID")?;
        if let Some(ts) = thread_ts {
            slack_ts(ts, "slack thread_ts")?;
        }
        SurfaceConversationRef::new(self.account(), channel_id, thread_ts.map(str::to_string))
    }
}

/// `<channel_id>:<ts>` — the two values Slack needs to address one message.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SlackMessageId {
    channel_id: String,
    ts: String,
}

impl SlackMessageId {
    pub fn new(channel_id: &str, ts: &str) -> Result<Self, SurfaceRefError> {
        slack_id(channel_id, "slack channel ID")?;
        slack_ts(ts, "slack message ts")?;
        Ok(Self {
            channel_id: channel_id.to_string(),
            ts: ts.to_string(),
        })
    }

    pub fn parse(message_id: &str) -> Result<Self, SurfaceRefError> {
        let (channel_id, ts) = message_id
            .split_once(MESSAGE_SEPARATOR)
            .ok_or(SurfaceRefError::Invalid("slack message ID"))?;
        Self::new(channel_id, ts)
    }

    pub fn channel_id(&self) -> &str {
        &self.channel_id
    }

    pub fn ts(&self) -> &str {
        &self.ts
    }

    pub fn encode(&self) -> String {
        format!("{}{MESSAGE_SEPARATOR}{}", self.channel_id, self.ts)
    }
}

fn slack_conversation(conversation: &SurfaceConversationRef) -> Result<(), SurfaceRefError> {
    SlackWorkspace::from_account(conversation.account()).map(drop)
}

/// A message posted (or received) with `ts` in this Slack conversation.
pub fn slack_message(
    conversation: &SurfaceConversationRef,
    ts: &str,
) -> Result<SurfaceMessageRef, SurfaceRefError> {
    slack_conversation(conversation)?;
    let id = SlackMessageId::new(conversation.conversation_id(), ts)?;
    SurfaceMessageRef::new(conversation.clone(), id.encode())
}

/// Where a reply goes: the conversation (a thread, when replying in one) and
/// optionally the message being answered.
pub fn slack_reply_target(
    conversation: &SurfaceConversationRef,
    quoted_ts: Option<&str>,
) -> Result<SurfaceReplyTarget, SurfaceRefError> {
    slack_conversation(conversation)?;
    let quoted = quoted_ts
        .map(|ts| SlackMessageId::new(conversation.conversation_id(), ts).map(|id| id.encode()))
        .transpose()?;
    SurfaceReplyTarget::new(conversation.clone(), quoted)
}

/// Whether the Slack surface in this repo delivers a capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SupportStatus {
    /// Implemented on the Slack surface with a named behavior test.
    Supported,
    /// Not implemented on the Slack surface. Slack may well allow it.
    Unsupported,
    /// Not shown to be possible on Slack at all; a parity blocker until proven.
    Unproven,
}

/// One row of the Slack column: the capability, its status, the issue that
/// owns it and the evidence the status rests on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityRow<K> {
    pub key: K,
    pub status: SupportStatus,
    pub tracking_issue: u32,
    pub basis: &'static str,
}

const fn row<K>(
    key: K,
    status: SupportStatus,
    tracking_issue: u32,
    basis: &'static str,
) -> CapabilityRow<K> {
    CapabilityRow {
        key,
        status,
        tracking_issue,
        basis,
    }
}

/// Slack-specific interaction capabilities named by #1282.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SlackInteraction {
    Buttons,
    Modals,
    Threads,
    SlashCommands,
    FileUpload,
    MessageEdit,
    EphemeralReplies,
    VoiceClip,
    LiveVoice,
}

impl SlackInteraction {
    pub const ALL: [SlackInteraction; 9] = [
        Self::Buttons,
        Self::Modals,
        Self::Threads,
        Self::SlashCommands,
        Self::FileUpload,
        Self::MessageEdit,
        Self::EphemeralReplies,
        Self::VoiceClip,
        Self::LiveVoice,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Buttons => "buttons",
            Self::Modals => "modals",
            Self::Threads => "threads",
            Self::SlashCommands => "slash_commands",
            Self::FileUpload => "file_upload",
            Self::MessageEdit => "message_edit",
            Self::EphemeralReplies => "ephemeral_replies",
            Self::VoiceClip => "voice_clip",
            Self::LiveVoice => "live_voice",
        }
    }

    pub fn status(self) -> SupportStatus {
        SLACK_INTERACTIONS
            .iter()
            .find(|row| row.key == self)
            .map(|row| row.status)
            .unwrap_or(SupportStatus::Unproven)
    }

    /// Typed refusal for anything not `Supported`.
    pub fn require(self) -> Result<(), SurfaceRefError> {
        match self.status() {
            SupportStatus::Supported => Ok(()),
            SupportStatus::Unsupported | SupportStatus::Unproven => {
                Err(SurfaceRefError::UnsupportedCapability(self.as_str()))
            }
        }
    }
}

/// The dated live-voice feasibility record (#1298), relative to the repo
/// root. Both voice rows cite it; `surface_contract` checks that the record's
/// `Recorded status:` line matches the `LiveVoice` row.
pub const LIVE_VOICE_FEASIBILITY_RECORD: &str = "docs/SLACK-LIVE-VOICE.md";

/// The Slack column for the shared capability set.
pub const SLACK_SHARED_CAPABILITIES: [CapabilityRow<SurfaceCapability>; 11] = [
    row(
        SurfaceCapability::Query,
        SupportStatus::Supported,
        1287,
        "`serve` runs Slack as an immediate interactive surface: owner DMs and control-channel \
         messages start a turn within a second through the durable inbox, beside or without \
         Discord and WhatsApp (interactive.rs, tests/interactive_surface.rs, \
         augmentagent-cli tests/slack_serve_cli.rs)",
    ),
    row(
        SurfaceCapability::Approve,
        SupportStatus::Supported,
        1289,
        "approval cards post to the owner DM or control channel and every decision (approve, \
         skip, revise, presets, missing info, recompose) runs the shared ApprovalActionHandler \
         with cross-surface redraw (approvals/, tests/approval_surface.rs); scheduling \
         controls are #1291",
    ),
    row(
        SurfaceCapability::Send,
        SupportStatus::Supported,
        1290,
        "approved contact replies (DM, group DM, channel, in thread) and owner-composed messages \
         are sent once, as the owner's own account through the Composio user connection, with \
         a per-action send ledger for retries (contact/, tests/contact_send.rs, \
         tests/contact_surface.rs); live-workspace identity is unverified",
    ),
    row(
        SurfaceCapability::Schedule,
        SupportStatus::Supported,
        1291,
        "schedule, reschedule, send now, cancel and back to queue on cards and as text \
         commands, with the resolved time and zone, fired once by the shared scheduler \
         through the contact send ledger (approvals/, tests/approval_scheduling.rs)",
    ),
    row(
        SurfaceCapability::ModelControl,
        SupportStatus::Supported,
        1292,
        "`model` (and `/jarvis model`) shows and sets the model per conversation on the \
         transport-neutral key, persisted across restarts and used by the next turn; `reset` \
         starts a new native session (commands/, tests/owner_commands.rs, \
         tests/owner_commands_surface.rs)",
    ),
    row(
        SurfaceCapability::ProcessControl,
        SupportStatus::Supported,
        1292,
        "`processes` lists and stops `claude` CLI processes through the cross-platform walker \
         and says so when it cannot run on the host; `cancel all` stops the running turn and \
         drops the queue (commands/, tests/owner_commands.rs, tests/owner_commands_surface.rs)",
    ),
    row(
        SurfaceCapability::MediaRead,
        SupportStatus::Supported,
        1293,
        "owner-sent images, text, PDF/DOCX and attachment-only messages reach the turn through \
         the shared inbound pipeline, downloaded only from Slack file hosts (inbound.rs, \
         tests/inbound_files.rs, tests/transport_download.rs)",
    ),
    row(
        SurfaceCapability::MediaWrite,
        SupportStatus::Supported,
        1294,
        "answers are split losslessly and generated files uploaded into the thread through the \
         outbox, once across restarts (delivery/, tests/delivery_outbox.rs, \
         tests/transport_upload.rs)",
    ),
    row(
        SurfaceCapability::History,
        SupportStatus::Supported,
        1296,
        "history-in-prompt providers read the turn's thread or DM, bounded and owner-only, and \
         subscribed conversations are searchable by person and channel (history.rs, \
         tests/conversation_history.rs, tests/search_identity.rs)",
    ),
    row(
        SurfaceCapability::Notifications,
        SupportStatus::Supported,
        1295,
        "digests, research, reminders, audit, health and review notices are routed per class \
         to the owner's DM or control channel through the durable Slack outbox (deduplicated, \
         paced, marked late after a suspension), Slack-only without Discord credentials; \
         broker notices reach Slack through the approval surfaces and loop output through \
         its destination (notify.rs, tests/notifications.rs); one-shot broker commands stay \
         Discord-only",
    ),
    row(
        SurfaceCapability::Voice,
        SupportStatus::Unproven,
        1298,
        "no documented Slack API lets an app join a huddle or carry call audio (checked \
         2026-09-29, docs/SLACK-LIVE-VOICE.md); voice clips (#1297) do not satisfy it",
    ),
];

/// The Slack column for Slack-specific interactions.
pub const SLACK_INTERACTIONS: [CapabilityRow<SlackInteraction>; 9] = [
    row(
        SlackInteraction::Buttons,
        SupportStatus::Supported,
        1289,
        "approval card buttons and the quick-refine select (block_actions), owner-gated and \
         resolved exactly once (approvals/, tests/approval_surface.rs)",
    ),
    row(
        SlackInteraction::Modals,
        SupportStatus::Supported,
        1289,
        "revise and missing-info modals (views.open + view_submission), with a fresh-message \
         fallback when the trigger expired (approvals/, tests/approval_surface.rs)",
    ),
    row(
        SlackInteraction::Threads,
        SupportStatus::Supported,
        1288,
        "each Slack thread (and a top-level control-channel message, which starts one) is a \
         persistent conversation with its own native session; answers, progress and \
         follow-ups stay in the thread (interactive.rs, harness.rs)",
    ),
    row(
        SlackInteraction::SlashCommands,
        SupportStatus::Supported,
        1292,
        "`/jarvis <command>` runs every owner command in the shared registry (help generated \
         from it), in a private lane that never queues behind a turn; other text still talks \
         to Jarvis (commands/, tests/owner_commands.rs, tests/owner_commands_surface.rs)",
    ),
    row(
        SlackInteraction::FileUpload,
        SupportStatus::Supported,
        1294,
        "files.getUploadURLExternal + completeUploadExternal, streamed from disk into the \
         thread (transport/web.rs, tests/transport_upload.rs, tests/delivery_outbox.rs)",
    ),
    row(
        SlackInteraction::MessageEdit,
        SupportStatus::Supported,
        1294,
        "chat.update for throttled turn progress and approval cards redrawn in place \
         (delivery/progress.rs, tests/delivery_progress.rs, tests/approval_surface.rs)",
    ),
    row(
        SlackInteraction::EphemeralReplies,
        SupportStatus::Supported,
        1287,
        "a non-owner gets one bounded chat.postEphemeral rejection and never reaches the \
         handler (interactive.rs, tests/interactive_surface.rs)",
    ),
    row(
        SlackInteraction::VoiceClip,
        SupportStatus::Supported,
        1297,
        "owner clips are transcribed into a turn in the same conversation with the \
         transcript shown first, and `voice on` answers with an uploaded audio file plus the \
         text through the Rust Deepgram/ElevenLabs adapter (voice/, interactive.rs, \
         tests/voice_surface.rs, tests/voice_tts_http.rs, tests/voice_clips.rs); live voice \
         is separate (#1298)",
    ),
    row(
        SlackInteraction::LiveVoice,
        SupportStatus::Unproven,
        1298,
        "no documented Slack API lets an app join a huddle or carry call audio; the Calls API \
         only registers a third-party call, so any route puts the audio outside Slack and \
         needs an owner decision (checked 2026-09-29, docs/SLACK-LIVE-VOICE.md); voice \
         clips (#1297) do not satisfy it",
    ),
];

/// The shared capabilities Slack may claim today: exactly the `Supported` rows.
pub fn slack_capabilities() -> SurfaceCapabilities {
    SurfaceCapabilities::new(
        SLACK_SHARED_CAPABILITIES
            .iter()
            .filter(|row| row.status == SupportStatus::Supported)
            .map(|row| row.key),
    )
}

/// Keys in `all` that have no row: non-empty means the column is incomplete.
pub fn missing_rows<K: Copy + PartialEq>(rows: &[CapabilityRow<K>], all: &[K]) -> Vec<K> {
    all.iter()
        .copied()
        .filter(|key| !rows.iter().any(|row| row.key == *key))
        .collect()
}

/// A capability that blocks full Slack parity and who owns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParityBlocker {
    pub name: &'static str,
    pub status: SupportStatus,
    pub tracking_issue: u32,
    pub basis: &'static str,
}

/// Every row that is not `Supported`, shared capabilities first.
pub fn parity_blockers() -> Vec<ParityBlocker> {
    let shared = SLACK_SHARED_CAPABILITIES
        .iter()
        .filter(|row| row.status != SupportStatus::Supported)
        .map(|row| ParityBlocker {
            name: row.key.as_str(),
            status: row.status,
            tracking_issue: row.tracking_issue,
            basis: row.basis,
        });
    let interactions = SLACK_INTERACTIONS
        .iter()
        .filter(|row| row.status != SupportStatus::Supported)
        .map(|row| ParityBlocker {
            name: row.key.as_str(),
            status: row.status,
            tracking_issue: row.tracking_issue,
            basis: row.basis,
        });
    shared.chain(interactions).collect()
}
