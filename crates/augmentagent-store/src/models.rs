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

/// A Composio-connected Google Drive account for a multi-tenant agent.
/// Lives only in a tenant's db; prod has none.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriveAccount {
    pub id: String,
    pub connection_id: String,
    pub entity_id: String,
    pub email: String,
    pub label: Option<String>,
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

/// One row per `(scope_kind, scope_value, account_entity_id)` — the
/// summarized voice profile injected into `draft_user_message` as a
/// `<tone_profile>` block. See issue #73.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToneProfile {
    pub id: String,
    /// `global` | `domain` | `recipient`.
    pub scope_kind: String,
    /// `*` for global, `acme.com` for domain, `jeremy@acme.com` for recipient.
    pub scope_value: String,
    /// Per-account so distinct Gmail identities don't blend voices. `None`
    /// means cross-account (only meaningful for `global`).
    pub account_entity_id: Option<String>,
    /// The Haiku-distilled descriptor (~120 tokens, JSON).
    pub summary: String,
    /// JSON array of `tone_examples.id` used in the last summarization.
    pub exemplar_ids: String,
    pub sample_count: i64,
    /// Snapshot of `sample_count` at last refresh; refresh fires when the
    /// delta crosses the staleness threshold (default 5).
    pub sample_count_at_refresh: i64,
    pub last_refreshed_at: i64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// One row per ingested sent-mail body (or per user-edit captured from a
/// `Sent` action). Feeds the per-scope summarizer in #73.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToneExample {
    pub id: String,
    /// `sent_backfill` | `user_edit` | `approved_clean`.
    pub source: String,
    pub action_id: Option<String>,
    pub message_id: Option<String>,
    pub account_entity_id: String,
    pub recipient_email: String,
    pub recipient_domain: String,
    pub subject: Option<String>,
    pub body: String,
    pub body_chars: i64,
    pub sent_at_ms: i64,
    pub ingested_at_ms: i64,
    pub weight: f64,
}

/// One row in `rate_events`. Persisted by the RateGovernor (#83) on every
/// outbound platform action — both the durable audit log and the source of
/// truth for sliding-window cap math after restart.
///
/// `status` is the snake-case form of the rate-governor outcome (`ok` |
/// `failed` | `rolled_back` | `suspicion`). `rolled_back` rows are
/// *excluded* from cap counting (the action never executed); the others
/// all burn quota.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateEvent {
    pub id: String,
    pub platform: String,
    pub action_kind: String,
    pub account_id: String,
    pub occurred_at_ms: i64,
    pub status: String,
    pub cause: String,
    pub target_id: Option<String>,
    pub meta_json: Option<String>,
}

/// One row in `rate_halts`. Per-platform circuit breaker, written when the
/// channel surfaces a suspicion signal (captcha, login challenge, 429, …).
/// `permit()` consults this before any cap math: while `paused_until_ms` is in
/// the future, every action on that platform is denied.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateHalt {
    pub platform: String,
    pub paused_until_ms: i64,
    pub reason: String,
    pub triggered_by_event_id: Option<String>,
    pub created_at_ms: i64,
    pub acknowledged_at_ms: Option<i64>,
}

/// One row in `rate_warmup`. Tracks when a `(platform, account_id)` pair
/// first started ramping. The warmup multiplier (#83 §4) is a pure function
/// of `now - warmup_started_at_ms`; the row is created lazily on first
/// `permit()` for an unknown account.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateWarmup {
    pub platform: String,
    pub account_id: String,
    pub warmup_started_at_ms: i64,
}

/// Aggregated audit row returned by `Store::rate_audit_query` (#83 §8).
/// One per `rate_events` row; the dashboard table / Discord embed render
/// directly from this shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateAuditRow {
    pub id: String,
    pub platform: String,
    pub action_kind: String,
    pub account_id: String,
    pub occurred_at_ms: i64,
    pub status: String,
    pub cause: String,
    pub target_id: Option<String>,
    pub meta_json: Option<String>,
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

/// A connected Telegram bot (#74). One row per bot the user has registered
/// via `augmentagent telegram-bot login`; the poller iterates active rows
/// each tick, loads each one's keyring slot, and long-polls `getUpdates` per
/// bot. `last_update_id` is the offset cursor handed to the next call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramBot {
    pub id: String,
    pub bot_id: i64,
    pub bot_username: String,
    pub owner_chat_id: i64,
    pub last_update_id: i64,
    pub active: bool,
    pub created_at_ms: i64,
}

/// #61 — LinkedIn 1st-degree connection sync cursor. One row per LinkedIn
/// account (keyed by the user's own `urn:li:fsd_profile:...`). `cursor_start`
/// resumes a paginated full sync that was interrupted; the two timestamps
/// drive the full-vs-delta decision (full = monthly, delta = daily).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkedInConnectionSync {
    pub account_id: String,
    pub last_full_sync_ms: Option<i64>,
    pub last_delta_sync_ms: Option<i64>,
    pub cursor_start: i64,
    pub last_synced_count: i64,
}

/// #62 — one row in the phone→person reverse index. Lets message-triage map
/// an inbound `+1415…` to an existing wiki page before it forks a new one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhoneIdentity {
    /// E.164-normalized (`+14155551234`).
    pub phone: String,
    pub person_slug: String,
    pub display_name: Option<String>,
    /// Provenance: `google_people` | `carddav` | `email_signature`.
    pub source: String,
}

/// A paired WhatsApp linked device (#74). One row per phone the user has
/// linked via `augmentagent whatsapp login`. The whatsmeow noise session
/// itself lives in the sidecar's own store + the keyring slot; this table is
/// the index the channel iterates and the place `session_status` is tracked
/// for re-pair detection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhatsappDevice {
    pub id: String,
    pub phone: String,
    pub device_jid: String,
    pub user_jid: String,
    pub paired_at_ms: i64,
    pub last_event_at_ms: i64,
    /// `paired` | `logged_out` — flipped to `logged_out` when the sidecar
    /// emits a `logged-out` event so the channel stops trying to send.
    pub session_status: String,
    pub active: bool,
    pub created_at_ms: i64,
}

/// #58.1 — one queued outbound post. Synthesized into a
/// `scheduled_post_fire` WorkItem by the serve-tick fire loop: at T-30min a
/// preview approval card is posted (`status` → `previewed`), at T-0 anything
/// still `previewed` (i.e. the user did not cancel) is published via the
/// per-platform Track-2.2 posting client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledPost {
    pub id: String,
    /// `twitter` | `linkedin` | `instagram` | …
    pub platform: String,
    pub body: String,
    /// JSON array of local media file paths, or `None` for a text post.
    pub media_paths: Option<String>,
    pub fire_at_ms: i64,
    /// `queued` | `previewed` | `posted` | `cancelled` | `failed`.
    pub status: String,
    /// Discord message id of the preview card once it's been surfaced.
    pub approval_msg: Option<String>,
    pub posted_at_ms: Option<i64>,
    /// Platform's post id once published (tweet id / share urn / media id).
    pub external_id: Option<String>,
    /// For tweetstorms / multi-part LI — parent `scheduled_posts.id`.
    pub thread_parent: Option<String>,
    pub created_at_ms: i64,
}

/// Lifecycle states for a [`ScheduledPost`]. String-typed in the DB so a
/// future state never needs a migration; this enum is the canonical set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduledPostStatus {
    Queued,
    Previewed,
    Posted,
    Cancelled,
    Failed,
}

impl ScheduledPostStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Previewed => "previewed",
            Self::Posted => "posted",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "queued" => Some(Self::Queued),
            "previewed" => Some(Self::Previewed),
            "posted" => Some(Self::Posted),
            "cancelled" => Some(Self::Cancelled),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// A user-scheduled recurring prompt/command (`/loop`, #104). The scheduler
/// ticks every active row on its `interval_secs` cadence. Minimal model
/// matching the `user_loops` table the store methods read/write.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserLoop {
    pub id: String,
    pub owner: String,
    pub channel: String,
    pub channel_ref: String,
    pub interval_secs: i64,
    pub prompt: String,
    pub status: String,
    pub last_run_ms: Option<i64>,
    pub last_status: Option<String>,
    pub fail_count: i64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}
