//! Discord broker for AugmentAgent: reply-approval cards (sqlite-backed,
//! never expire) + wiki query message listener.
//!
//! Design note — persistent approvals: approval state lives in sqlite, NOT in
//! an in-process map. When the user clicks Approve / Revise / Skip, the event
//! handler looks up the action in the database and delegates to an injected
//! `ApprovalActionHandler` for the actual work (send, revise, delete draft).
//! This means cards never "expire" — they stay live as long as the action is
//! still `pending`, across daemon restarts, unlimited timeouts.

pub mod attachments;
mod broker;
mod custom_id;
mod event_handler;
mod journal_cmd;
mod layout;
mod loops;
mod nudge;
mod process_loops;
mod status_bus;
mod surface;
mod presets;
// #501 — deterministic send-time parsing. Public module: the event handler's
// select/modal arms resolve here, and `augmentagent-channel-core` re-exports
// it for the query-mode `--send-at` flag (#502) — channel-core depends on
// this crate, so the shared implementation must live on this side.
pub mod timeparse;

pub use broker::{DiscordApprovalBroker, DiscordConfig};
// #501 — `append_envelope_markers` is shared with the CLI's Back-to-queue
// repost so it renders the same To/cc/bcc decoration as the Revise repost.
pub use event_handler::{append_envelope_markers, chunk_for_discord};
pub use journal_cmd::{parse_journal_command, JournalCmd, JOURNAL_NOT_CONFIGURED, JOURNAL_USAGE};
// #35 Phase 5: the email channel appends the needs-input marker to the
// persisted draft via this; the card decodes it on render. `NeedsInput` is
// re-exported for the channel/test surface.
pub use layout::{append_needs_input_marker, approval_message, split_needs_input, NeedsInput};
// #785: the drafter emits the assumed-facts marker; the channel splits it off
// the Gmail body and the card renders it as the "⚠ Assumes" field.
pub use layout::{append_assumes_marker, split_assumes, strip_assumes_for_send};
// #501 — scheduled-notice layout, for brokers/tests outside this crate.
pub use layout::{scheduled_notice_message, schedule_modal, SCHEDULE_CUSTOM_VALUE};
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
    /// Schedule armed (#501) — `pending → scheduled` at `at_ms`. `local` is
    /// the pre-formatted owner-local fire time for the ephemeral ack; the
    /// card is retired and a scheduled notice takes its place.
    Scheduled { at_ms: i64, local: String },
    /// Back to queue succeeded (#501) — schedule disarmed
    /// (`scheduled → pending`, `scheduledAtMs` NULLed), approval card
    /// reposted.
    Unscheduled,
    /// Cancel succeeded (#501) — scheduled send cancelled
    /// (`scheduled → rejected`), Gmail draft deleted like Skip.
    CancelledSchedule,
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
            attachments: Vec::new(),
            to: String::new(),
            cc: String::new(),
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

    /// #501 — post the compact scheduled-send notice (Send Now / Back to
    /// queue / Cancel) for a freshly armed schedule. `to_display` is the
    /// resolved send target (the #473 envelope To when recorded, else the
    /// card's From). Returns the Discord `(channel_id, message_id)` pair so
    /// the caller can persist the notice pointers on the action row (the
    /// engine deletes the notice at fire/cancel time). Default `Ok(None)`:
    /// brokers without a notice surface (Noop, tests) arm schedules fine —
    /// there's just no message to clean up later.
    async fn post_scheduled_notice(
        &self,
        action_id: &str,
        email: &Email,
        sends_at_local: &str,
        sends_at_ms: i64,
        to_display: &str,
    ) -> Result<Option<(u64, u64)>, ApprovalError> {
        let _ = (action_id, email, sends_at_local, sends_at_ms, to_display);
        Ok(None)
    }

    /// #501 — post an approval card and return its Discord
    /// `(channel_id, message_id)` pair, honoring the persisted redraft count
    /// (so a refined-to-cap card is reposted WITHOUT the quick-refine row —
    /// `post_approval` hardcodes count 0). Back-to-queue posts the card
    /// BEFORE its CAS and needs the ids to take the card back down when the
    /// CAS loses (#501 review). Default: delegates to
    /// [`Self::post_approval`] and returns `Ok(None)` — no ids, nothing to
    /// roll back, fine for brokers with no real message surface.
    async fn post_approval_card(
        &self,
        action_id: &str,
        email: &Email,
        draft: &str,
        redraft_count: u32,
    ) -> Result<Option<(u64, u64)>, ApprovalError> {
        let _ = redraft_count;
        self.post_approval(action_id, email, draft).await?;
        Ok(None)
    }

    /// #501 — delete a message this broker previously posted (the scheduled
    /// notice, at fire/cancel/send-now time). Best-effort by convention at
    /// every call site; the default no-op keeps notice-less brokers compiling.
    async fn delete_message(
        &self,
        channel_id: u64,
        message_id: u64,
    ) -> Result<(), ApprovalError> {
        let _ = (channel_id, message_id);
        Ok(())
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

/// Bridge into the ShadowNote journaling flow (#428):
/// this crate owns the `!journal` grammar + dispatch, the
/// cli implements the encrypt/AppSync/wiki-ingest plumbing, no circular
/// dep and no AWS dependencies here.
///
/// Both methods return the user-facing reply line. The `Err` string is
/// ALSO user-facing and must carry enough of the entry text that a failed
/// save never silently loses what the user wrote.
#[async_trait]
pub trait JournalOps: Send + Sync {
    /// `!journal <text>` — save the raw message text as an entry (the
    /// impl wraps it into the app's paragraph HTML).
    async fn save_text(&self, title: Option<String>, text: &str) -> Result<String, String>;

    /// `!journal done [title]` — compose an entry from the conversation
    /// excerpt and save it. `title_override` wins over the composed title.
    async fn compose_and_save(
        &self,
        history: &str,
        title_override: Option<String>,
    ) -> Result<String, String>;
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

    /// #501 — arm a scheduled send: `pending → scheduled` at `at_ms`
    /// (epoch-ms, already resolved from the select token or modal text). The
    /// impl owns the central time guard and the CAS. Default: unsupported,
    /// so handlers without a schedule surface keep compiling.
    async fn schedule(&self, action_id: &str, at_ms: i64) -> ApprovalActionOutcome {
        let _ = (action_id, at_ms);
        ApprovalActionOutcome::Failed {
            message: "scheduling is not supported by this handler".into(),
        }
    }

    /// #501 — "Send Now" on the scheduled notice: direct
    /// `scheduled → sending` CAS into the Approve send tail. NEVER routes
    /// through `pending` (that would re-enter the nudge queue and, after
    /// #502, re-arm the proposal).
    async fn send_now(&self, action_id: &str) -> ApprovalActionOutcome {
        let _ = action_id;
        ApprovalActionOutcome::Failed {
            message: "scheduling is not supported by this handler".into(),
        }
    }

    /// #501 — "Cancel" on the scheduled notice: `scheduled → rejected` with
    /// the Gmail draft deleted (Skip convention).
    async fn cancel_schedule(&self, action_id: &str) -> ApprovalActionOutcome {
        let _ = action_id;
        ApprovalActionOutcome::Failed {
            message: "scheduling is not supported by this handler".into(),
        }
    }

    /// #501 — "Back to queue" on the scheduled notice: `scheduled → pending`
    /// (schedule disarmed) with the approval card reposted.
    async fn back_to_queue(&self, action_id: &str) -> ApprovalActionOutcome {
        let _ = action_id;
        ApprovalActionOutcome::Failed {
            message: "scheduling is not supported by this handler".into(),
        }
    }

    /// #501 — true while the action still holds a live schedule (`scheduled`
    /// or the in-flight `sending` claim). Backs the verb-aware startup
    /// sweep: a scheduled NOTICE is deleted only when this is false, while
    /// actionable cards keep using [`Self::is_resolved`] — a blanket
    /// exemption either way would immortalize one message kind or delete the
    /// other at every restart. Default `false` (no schedule surface ⇒ any
    /// leftover notice is stale).
    async fn is_schedule_live(&self, action_id: &str) -> bool {
        let _ = action_id;
        false
    }
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
