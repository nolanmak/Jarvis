use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Email {
    pub message_id: String,
    pub thread_id: Option<String>,
    pub from: String,
    pub subject: String,
    pub body: String,
    pub date: String,
    pub account_entity_id: Option<String>,
    /// Source platform: `gmail`, `linkedin`, `slack`, `discord`, `whatsapp`, `twitter`, `instagram`.
    /// Free-form string — no DB CHECK constraint — so new platforms don't require a schema migration.
    #[serde(default = "default_platform")]
    pub platform: String,
    /// Interaction kind: `dm`, `post_reply`, `post_engagement`, `digest_item`.
    /// Separates reactive 1:1 DMs from proactive feed engagement and read-only digest items.
    #[serde(default = "default_kind")]
    pub kind: String,
}

fn default_platform() -> String {
    "gmail".into()
}

fn default_kind() -> String {
    "dm".into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionStatus {
    Pending,
    Approved,
    Rejected,
    Sent,
    Error,
    TimedOut,
    Skipped,
    Flagged,
    DryRun,
}

impl ActionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
            Self::Sent => "sent",
            Self::Error => "error",
            Self::TimedOut => "timed_out",
            Self::Skipped => "skipped",
            Self::Flagged => "flagged",
            Self::DryRun => "dry_run",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriageResult {
    Reply,
    Skip,
    Flag,
    /// Feed engagement: agent drafts a supportive comment/reaction on a friend's post.
    Engage,
    /// Firehose item: ingested for digest roll-up, never prompts the user directly.
    DigestOnly,
}

impl TriageResult {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reply => "reply",
            Self::Skip => "skip",
            Self::Flag => "flag",
            Self::Engage => "engage",
            Self::DigestOnly => "digest_only",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionRecord {
    pub id: String,
    pub message_id: String,
    pub thread_id: Option<String>,
    pub from_email: String,
    pub subject: String,
    pub original_body: Option<String>,
    pub draft_body: Option<String>,
    pub status: String,
    pub error_message: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub connection_id: Option<String>,
    pub entity_id: String,
    pub email: String,
    pub active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LearnedPattern {
    pub pattern_type: String,
    pub pattern: String,
    pub action: String,
    pub reason: String,
}

/// Routing mode for a channel subscription. Drives how the Discord (and
/// future Slack/WhatsApp/Twitter) channel poller dispatches each incoming
/// message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionMode {
    /// Full triage → draft → approval card → send (DM pipeline).
    Priority,
    /// Store messages as they arrive; once-daily Claude summary.
    Digest,
    /// Raw persistence; no Claude, no approval cards.
    StoreOnly,
}

impl SubscriptionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Priority => "priority",
            Self::Digest => "digest",
            Self::StoreOnly => "store_only",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "priority" => Some(Self::Priority),
            "digest" => Some(Self::Digest),
            "store_only" => Some(Self::StoreOnly),
            _ => None,
        }
    }
}

/// A watched channel (Discord DM, Discord guild channel, Slack channel/DM)
/// and the mode it's polled under.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelSubscription {
    pub id: String,
    pub platform: String,
    pub channel_id: String,
    pub display_name: String,
    pub mode: SubscriptionMode,
    pub active: bool,
    /// Platform-specific account this subscription belongs to. For Slack this
    /// is the `team_id` (workspace); channels with the same `channel_id` can
    /// coexist across workspaces. `None` for single-account platforms (Discord
    /// user-token, legacy Gmail).
    pub account_id: Option<String>,
    /// Snowflake of the newest message we've already seen — used for
    /// `GET /channels/{id}/messages?after=<this>` polling. `None` on a fresh
    /// subscription (next poll grabs the most recent `limit` messages).
    pub last_seen_message_id: Option<String>,
    /// Timestamp (ms since epoch) of the last digest post for this subscription.
    /// Only meaningful for `Digest` mode. Used to skip subscriptions that
    /// already got a digest in the current window.
    pub last_digest_at_ms: Option<i64>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// A connected Slack workspace, persisted alongside `gmail_accounts`. One row
/// per OAuth connection; the poller iterates these each tick to build a
/// per-workspace `SlackClient` from its Keychain entry keyed by `team_id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlackWorkspace {
    pub id: String,
    pub team_id: String,
    pub team_name: String,
    pub entity_id: String,
    pub connection_id: String,
    pub user_id: String,
    pub active: bool,
    pub created_at_ms: i64,
}
