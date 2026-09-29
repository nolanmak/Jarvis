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
//! yet shown to be possible at all (`Unproven`), and which issue owns it. The
//! shared matrix and command registry from #1226 have not landed; when they
//! do, these tables are the input for the Slack column, not a second layer.

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

const NOT_WIRED: &str = "no Slack interactive surface exists; the crate only polls subscribed \
     conversations through Composio and posts plain text (channel.rs, api.rs)";

/// The Slack column for the shared capability set.
pub const SLACK_SHARED_CAPABILITIES: [CapabilityRow<SurfaceCapability>; 11] = [
    row(SurfaceCapability::Query, SupportStatus::Unsupported, 1287, NOT_WIRED),
    row(
        SurfaceCapability::Approve,
        SupportStatus::Unsupported,
        1289,
        "Slack contact-reply approvals are posted through the Discord ApprovalBroker (channel.rs)",
    ),
    row(
        SurfaceCapability::Send,
        SupportStatus::Unsupported,
        1290,
        "SlackClient::send_message exists but no owner-driven send path uses the shared refs",
    ),
    row(SurfaceCapability::Schedule, SupportStatus::Unsupported, 1291, NOT_WIRED),
    row(SurfaceCapability::ModelControl, SupportStatus::Unsupported, 1292, NOT_WIRED),
    row(SurfaceCapability::ProcessControl, SupportStatus::Unsupported, 1292, NOT_WIRED),
    row(SurfaceCapability::MediaRead, SupportStatus::Unsupported, 1293, NOT_WIRED),
    row(SurfaceCapability::MediaWrite, SupportStatus::Unsupported, 1294, NOT_WIRED),
    row(
        SurfaceCapability::History,
        SupportStatus::Unsupported,
        1296,
        "SlackClient::fetch_messages ingests contacts; no owner conversation history is keyed by the shared refs",
    ),
    row(SurfaceCapability::Notifications, SupportStatus::Unsupported, 1295, NOT_WIRED),
    row(
        SurfaceCapability::Voice,
        SupportStatus::Unproven,
        1298,
        "live two-way audio through a Slack app is unverified; voice clips (#1297) do not satisfy it",
    ),
];

/// The Slack column for Slack-specific interactions.
pub const SLACK_INTERACTIONS: [CapabilityRow<SlackInteraction>; 9] = [
    row(
        SlackInteraction::Buttons,
        SupportStatus::Unsupported,
        1289,
        NOT_WIRED,
    ),
    row(
        SlackInteraction::Modals,
        SupportStatus::Unsupported,
        1289,
        NOT_WIRED,
    ),
    row(
        SlackInteraction::Threads,
        SupportStatus::Unsupported,
        1288,
        "thread identity is mapped here (thread_ts) but no runtime reads or posts in threads",
    ),
    row(
        SlackInteraction::SlashCommands,
        SupportStatus::Unsupported,
        1292,
        NOT_WIRED,
    ),
    row(
        SlackInteraction::FileUpload,
        SupportStatus::Unsupported,
        1294,
        NOT_WIRED,
    ),
    row(
        SlackInteraction::MessageEdit,
        SupportStatus::Unsupported,
        1294,
        NOT_WIRED,
    ),
    row(
        SlackInteraction::EphemeralReplies,
        SupportStatus::Unsupported,
        1287,
        NOT_WIRED,
    ),
    row(
        SlackInteraction::VoiceClip,
        SupportStatus::Unsupported,
        1297,
        NOT_WIRED,
    ),
    row(
        SlackInteraction::LiveVoice,
        SupportStatus::Unproven,
        1298,
        "whether a Slack app can exchange live audio with the owner is unverified",
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
