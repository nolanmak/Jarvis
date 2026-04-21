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
