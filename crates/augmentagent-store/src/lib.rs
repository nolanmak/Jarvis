//! Product-state store backed by the shared `data.db` sqlite file.
//!
//! Schema is owned by the Node tree (`src/db.ts`); this crate never runs migrations.
//! Opens the database in WAL journal mode so the Express dashboard can read
//! concurrently.

// #1289 — durable approval-card pointers per surface.
pub mod approval_cards;
pub mod daemon_report;
pub mod delivery;
pub mod imessage;
pub mod models;
pub mod notes;
pub mod owner;
pub mod redact;
// #1290 — Slack contact send targets and the per-action send ledger.
pub mod slack_contact;
// #1296 — live/poll reconciliation ledger for subscribed Slack messages.
pub mod slack_ingest;
mod store;
pub mod surface;
// #1292 — reset, cancel-all and loop pause/resume behind owner commands.
pub mod surface_commands;
pub mod surface_health;
// #1297 — per-conversation reply mode (text or spoken) behind `voice on|off`.
pub mod surface_reply_modes;

pub use imessage::{
    ImessageOutboxItem, ImessageOutboxStatus, ImessageSendOutcome, ImessageTargetKind,
    NewImessageOutboxItem,
};
pub use surface::{
    SurfaceAccountRef, SurfaceCapabilities, SurfaceCapability, SurfaceConversationRef,
    SurfaceMessageRef, SurfaceOwnerRef, SurfacePlatform, SurfaceRefError, SurfaceReplyTarget,
    SurfaceTurnRef,
};

pub use models::{
    Account, ActionRecord, ActionStatus, AgentPrRun, AgentRepo, ChannelSubscription,
    ConnectionRequestRow, DriveAccount, Email, FriendWatch, LearnedPattern, LinkedInConnectionSync,
    OwnPost, PhoneIdentity, RateAuditRow, RateEvent, RateHalt, RateWarmup, ScheduledPost,
    ScheduledPostStatus, SlackWorkspace, SocialapiAccount, SocialapiWebhookEvent, SubscriptionMode,
    TelegramBot, ToneExample, ToneProfile, TriageResult, UserLoop, WhatsappDevice,
};
pub use store::{
    ActionCodeModeFields, ActionWithEmail, DiscordConversation, JournalSyncCursor,
    NativeConversation, PendingActionRow, PendingNudge, RetryableReply, RevisionRecord, Store,
    StoreError, StoreResult, SurfaceTurnResolution, SurfaceTurnState, SurfaceTurnStatus,
    NUDGE_INTERVAL_MS,
    WhatsappInboundEvent, WhatsappOwnerConfig,
};

/// Re-exported so extension crates (`augmentagent-proactive`, …) can write
/// `Store::with_conn` closures without taking a direct `rusqlite` dep that
/// could drift from the version this crate links.
pub use rusqlite;
