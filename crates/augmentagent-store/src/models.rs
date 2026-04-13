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
}

impl TriageResult {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reply => "reply",
            Self::Skip => "skip",
            Self::Flag => "flag",
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
