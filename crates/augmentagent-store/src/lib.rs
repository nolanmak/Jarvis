//! Product-state store backed by the shared `data.db` sqlite file.
//!
//! Schema is owned by the Node tree (`src/db.ts`); this crate never runs migrations.
//! Opens the database in WAL journal mode so the Express dashboard can read
//! concurrently.

pub mod models;
mod store;

pub use models::{
    Account, ActionRecord, ActionStatus, AgentPrRun, AgentRepo, ChannelSubscription,
    ConnectionRequestRow, DriveAccount, Email, FriendWatch, InvoiceDraft, LearnedPattern,
    LinkedInConnectionSync, OwnPost, PhoneIdentity, RateAuditRow,
    RateEvent, RateHalt, RateWarmup, ScheduledPost, ScheduledPostStatus, SlackWorkspace,
    SubscriptionMode, TelegramBot, ToneExample,
    ToneProfile, TriageResult, UserLoop, WhatsappDevice,
};
pub use store::{
    ActionCodeModeFields, ActionWithEmail, PendingNudge, RetryableReply, RevisionRecord, Store,
    StoreError, StoreResult, NUDGE_INTERVAL_MS,
};

/// Re-exported so extension crates (`augmentagent-proactive`, …) can write
/// `Store::with_conn` closures without taking a direct `rusqlite` dep that
/// could drift from the version this crate links.
pub use rusqlite;
