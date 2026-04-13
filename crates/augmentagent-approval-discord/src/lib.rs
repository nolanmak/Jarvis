//! Discord approval broker for AugmentAgent reply-draft gating.
//!
//! Match-Node recovery mode: pending approvals live in-process only. A restart
//! abandons any in-flight Discord threads (rows stay `pending` in sqlite;
//! resolve via dashboard). Stale button clicks post-restart reply "expired".

mod broker;
mod custom_id;
mod event_handler;
mod layout;

pub use broker::{DiscordApprovalBroker, DiscordConfig};

use async_trait::async_trait;
use augmentagent_store::Email;
use thiserror::Error;

#[derive(Debug, Clone)]
pub enum ApprovalOutcome {
    Approved { final_draft: String },
    Revise { feedback: String },
    Skipped,
}

#[derive(Debug, Error)]
pub enum ApprovalError {
    #[error("timed out")]
    TimedOut,
    #[error("discord: {0}")]
    Discord(String),
    #[error("serenity: {0}")]
    Serenity(#[from] serenity::Error),
    #[error("not ready")]
    NotReady,
}

#[async_trait]
pub trait ApprovalBroker: Send + Sync {
    /// Post an approval request to Discord and await the user decision.
    /// `initial_draft` is what we show; the final draft returned may have been
    /// revised via the modal, so callers should use `ApprovalOutcome::Approved.final_draft`.
    async fn request(
        &self,
        action_id: &str,
        email: &Email,
        initial_draft: &str,
    ) -> Result<ApprovalOutcome, ApprovalError>;
}

/// Phase-1 no-op broker; errors on request so callers can treat reply as dry-run.
pub struct NoopBroker;

#[async_trait]
impl ApprovalBroker for NoopBroker {
    async fn request(&self, _: &str, _: &Email, _: &str) -> Result<ApprovalOutcome, ApprovalError> {
        Err(ApprovalError::NotReady)
    }
}
