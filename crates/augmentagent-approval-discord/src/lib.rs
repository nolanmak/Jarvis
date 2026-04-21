//! Discord broker for AugmentAgent: reply-approval cards (sqlite-backed,
//! never expire) + wiki query message listener.
//!
//! Design note — persistent approvals: approval state lives in sqlite, NOT in
//! an in-process map. When the user clicks Approve / Revise / Skip, the event
//! handler looks up the action in the database and delegates to an injected
//! `ApprovalActionHandler` for the actual work (send, revise, delete draft).
//! This means cards never "expire" — they stay live as long as the action is
//! still `pending`, across daemon restarts, unlimited timeouts.

mod broker;
mod custom_id;
mod event_handler;
mod layout;

pub use broker::{DiscordApprovalBroker, DiscordConfig};
pub use event_handler::chunk_for_discord;

use async_trait::async_trait;
use augmentagent_store::Email;
use thiserror::Error;

/// Outcome of an approval-action handler call. Drives the Discord ack shown
/// to the user.
///
/// `Revised` carries a full Email for card re-posting; other variants are
/// small. Boxing would ripple through every construction + pattern match,
/// and this enum is constructed once per user click — variant-size
/// imbalance isn't worth the churn.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum ApprovalActionOutcome {
    /// No such action in the db.
    NotFound,
    /// Action already in a terminal state (sent / rejected / permanent_error).
    AlreadyResolved { status: String },
    /// Approve succeeded — email sent, action marked Sent.
    Approved,
    /// Skip succeeded — draft deleted, action marked Rejected.
    Skipped,
    /// Revise succeeded — a new draft was created and should be re-posted as
    /// a fresh approval card with the same `action_id`.
    Revised { email: Email, draft: String },
    /// The handler attempted the action but hit an error (transient or
    /// permanent). Show the message to the user; the action stays pending so
    /// they can retry.
    Failed { message: String },
}

#[derive(Debug, Error)]
pub enum ApprovalError {
    #[error("discord: {0}")]
    Discord(String),
    #[error("serenity: {0}")]
    Serenity(#[from] serenity::Error),
    #[error("not ready")]
    NotReady,
}

/// Post a fresh approval card to Discord. Non-blocking: returns once the
/// Discord API round-trip finishes, not when the user clicks a button.
#[async_trait]
pub trait ApprovalBroker: Send + Sync {
    async fn post_approval(
        &self,
        action_id: &str,
        email: &Email,
        draft: &str,
    ) -> Result<(), ApprovalError>;

    /// Post a simple "heads up" notice for a triage-flagged email. No buttons,
    /// no draft — the user is expected to open Gmail themselves if they want
    /// to reply. This is the reach-out channel for emails that matter but
    /// don't warrant auto-drafting.
    async fn post_flag_notice(
        &self,
        email: &Email,
        reason: &str,
    ) -> Result<(), ApprovalError>;
}

/// No-op broker for dry-run mode; returns immediately.
pub struct NoopBroker;

#[async_trait]
impl ApprovalBroker for NoopBroker {
    async fn post_approval(
        &self,
        _: &str,
        _: &Email,
        _: &str,
    ) -> Result<(), ApprovalError> {
        Ok(())
    }

    async fn post_flag_notice(
        &self,
        _: &Email,
        _: &str,
    ) -> Result<(), ApprovalError> {
        Ok(())
    }
}

/// Executes the user's button click against the product side (gmail send /
/// delete / reasoner redraft) and returns the outcome. The CLI wires the
/// concrete impl with Store + GmailApi + Reasoner.
///
/// All methods take an `action_id` and re-hydrate the email/draft from the
/// sqlite row; we never trust in-process state to survive. This is what makes
/// old Discord cards still work after a daemon restart.
#[async_trait]
pub trait ApprovalActionHandler: Send + Sync {
    async fn approve(&self, action_id: &str) -> ApprovalActionOutcome;
    async fn revise(&self, action_id: &str, feedback: &str) -> ApprovalActionOutcome;
    async fn skip(&self, action_id: &str) -> ApprovalActionOutcome;
}

/// Plugged into the broker to answer wiki queries that arrive as Discord messages.
#[async_trait]
pub trait QueryHandler: Send + Sync {
    async fn answer(&self, question: &str) -> anyhow::Result<String>;
}
