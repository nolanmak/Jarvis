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
mod loops;
mod nudge;
mod process_loops;
mod status_bus;
mod surface;
mod presets;

pub use broker::{DiscordApprovalBroker, DiscordConfig};
pub use event_handler::{chunk_for_discord, post_invoice_draft_card};
// #35 Phase 5: the email channel appends the needs-input marker to the
// persisted draft via this; the card decodes it on render. `NeedsInput` is
// re-exported for the channel/test surface.
pub use layout::{append_needs_input_marker, split_needs_input, NeedsInput};
pub use loops::{
    handle_loop_command, match_loop_prefix, max_active_per_user, min_interval_secs,
    next_cron_firing_ms, normalize_and_validate_cron, parse_interval, pause_after_failures,
    validate_tz, LoopCommandParser, LoopPoster, LoopRunner, LoopScheduler, ParsedLoop,
};
pub use nudge::NudgeScheduler;
pub use status_bus::{StatusBus, StatusChanged};
pub use surface::{ApprovalSurface, ComposedSurface};
pub use presets::{Preset, MAX_REDRAFT_ITERATIONS, PRESETS};

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

    /// Post a daily digest summary to the approvals channel. `title` describes
    /// the source (e.g. "#general in Code & Coffee"), `body` is the
    /// Claude-generated summary. Default implementation forwards to
    /// `post_flag_notice` so brokers not yet updated still surface something;
    /// concrete brokers should override with a dedicated digest embed.
    async fn post_digest(
        &self,
        title: &str,
        body: &str,
    ) -> Result<(), ApprovalError> {
        // Fallback: synthesize a minimal Email + reason. Channels relying on
        // this must render it readably.
        let email = Email {
            message_id: format!("digest:{title}"),
            thread_id: None,
            from: title.to_string(),
            subject: format!("[digest] {title}"),
            body: body.to_string(),
            date: String::new(),
            account_entity_id: None,
            platform: "discord".into(),
            kind: "digest_item".into(),
        };
        self.post_flag_notice(&email, body).await
    }
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

/// Bridge into the weekly Orchid invoice flow (lives in `augmentagent-cli`).
/// The discord crate depends on this trait, the cli depends on the discord
/// crate and implements the trait — keeps the python-shell + config-read
/// logic in a single place (`cli::invoice`) while letting the discord
/// event handler drive Approve / Reject without a circular dep.
///
/// Both methods refer to a Sun→Sun billing window keyed by its closing
/// Sunday; the implementation reads recipient / counter / sending-entity
/// from the shared store.
#[async_trait]
pub trait InvoiceOps: Send + Sync {
    /// Generate the PDF for the given closing Sunday (defaults to the most
    /// recent Sunday when None). Returns `(number, pdf_path, week_start)`.
    async fn draft_pdf(
        &self,
        week_end: Option<chrono::NaiveDate>,
    ) -> anyhow::Result<InvoiceDraftPdf>;

    /// Real-send path. Calls the same code path as `augmentagent invoice run
    /// --dry-run false`: emails the PDF, advances the counter, records the
    /// billed week. The authoritative invoice number is peeked at send time
    /// (the card's number is only a tentative preview), so several pending
    /// weekly drafts each get a distinct sequential number on approval (#330).
    async fn send(
        &self,
        week_end: chrono::NaiveDate,
    ) -> anyhow::Result<String>;
}

/// What [`InvoiceOps::draft_pdf`] returns: enough to post a Discord card
/// (PDF path + invoice number + the rendered week window).
#[derive(Debug, Clone)]
pub struct InvoiceDraftPdf {
    pub number: u32,
    pub week_start: chrono::NaiveDate,
    pub week_end: chrono::NaiveDate,
    pub pdf_path: std::path::PathBuf,
    pub recipient: String,
    /// Exact outgoing email subject + body, shown on the approval card so the
    /// approver sees what gets emailed (#346).
    pub subject: String,
    pub body: String,
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
    /// True if the action exists and is in a terminal state (anything other
    /// than `Pending` / `DryRun`). Backs the startup scrollback sweep that
    /// deletes stale approval cards from previous runs.
    async fn is_resolved(&self, action_id: &str) -> bool;
}

/// Plugged into the broker to answer wiki queries that arrive as Discord messages.
///
/// Implementations receive an audit context per request so the underlying
/// reasoner can populate `ReasonerOpts.session_id` + `ReasonerOpts.audit_notifier`
/// without this crate having to depend on `augmentagent-channel-core` at
/// the type level (channel-core already depends on us — see
/// `engagement.rs::ApprovalBroker` — so a back-reference would cycle the
/// workspace). The `WikiQuerier` impl in the CLI crate is the natural
/// bridge: it sees both serenity (for `http` / `channel_id`) and
/// channel-core (for `AuditNotifier`), and assembles the real notifier
/// there (#132 / #201).
#[async_trait]
pub trait QueryHandler: Send + Sync {
    async fn answer(&self, ctx: &AuditCtx, question: &str) -> anyhow::Result<String>;
}

/// Per-request audit context handed to [`QueryHandler::answer`].
///
/// Holds the logical session id (typically `format!("{channel}:{msg}")`)
/// plus the raw serenity bits a Discord-side `AuditNotifier`
/// implementation needs to post side-channel messages back to the
/// originating channel. The CLI crate's `WikiQuerier` constructs the
/// real notifier from these bits, since this crate cannot depend on
/// `augmentagent-channel-core`'s `AuditNotifier` trait without a
/// workspace cycle.
pub struct AuditCtx {
    pub session_id: String,
    pub http: Option<std::sync::Arc<serenity::http::Http>>,
    pub channel_id: Option<serenity::model::id::ChannelId>,
}

impl AuditCtx {
    /// Build an empty context — useful for tests / non-Discord callers
    /// that still need to satisfy the [`QueryHandler::answer`] signature.
    pub fn empty() -> Self {
        Self {
            session_id: String::from("-"),
            http: None,
            channel_id: None,
        }
    }
}
