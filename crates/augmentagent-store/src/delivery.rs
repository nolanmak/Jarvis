//! #1285 / #1229 — durable delivery for chat surfaces: an inbound event log,
//! an outbox and bounded gap catch-up, keyed by the transport-neutral refs in
//! [`crate::surface`] so Slack, WhatsApp and Discord share one implementation.
//!
//! ## Inbound log (`surface_inbound_events`)
//!
//! A transport calls [`Store::record_inbound_event`] for every accepted event
//! and acknowledges it to the provider only after that returns `Ok`. The row
//! is unique per `(platform, account, event_id)`, so provider redeliveries and
//! catch-up replays are reported as [`InboundRecordOutcome::Duplicate`] and
//! never become a second turn. For messages, use the message identity (for
//! Slack `<channel>:<ts>`) as `event_id` so a live event and the same message
//! fetched from history deduplicate.
//!
//! States: `received → claimed → handled`, or `dead_letter` once an event has
//! been claimed `max_attempts` times without being handled. A conversation
//! (thread included) is processed serially in provider order: an event is
//! claimable only when nothing earlier in its conversation is still open and
//! nothing in it is claimed. After a restart, [`Store::recover_surface_delivery`]
//! returns interrupted claims to `received`; the replay carries
//! `attempt > 1` so the dispatcher can check its own effects (the native turn
//! store already refuses a turn whose previous run is uncertain).
//!
//! ## Outbox (`surface_outbox`)
//!
//! Every outbound post, update, upload or interaction response is queued with
//! an idempotency key unique per platform account. States: `queued →
//! sending → sent`; a retryable failure goes to `failed` with a backed-off
//! `next_attempt_at_ms`; exhaustion or a permanent error goes to
//! `dead_letter`; the owner can `abandon` a send. Sends in one conversation
//! go out in enqueue order: an open send holds every later one.
//!
//! A send that was `sending` when the process died may have reached the
//! provider. Recovery moves it to `reconcile`, which is never claimed for a
//! resend. [`reconcile_outbound_sends`] asks the transport's
//! [`SendReconciler`] whether the provider has it: delivered sends become
//! `sent` with their provider message ID, sends proven absent are requeued,
//! and unknown outcomes stay in `reconcile` (visible in status) rather than
//! being resent blindly.
//!
//! An interaction response whose handle has expired (for example a Slack
//! `response_url` after the host slept) is not attempted through the handle:
//! at claim time it is rewritten to a normal `post` and flagged `fell_back`.
//!
//! ## Gap catch-up (`surface_cursors`)
//!
//! Each conversation keeps a monotonic last-seen cursor. After a reconnect,
//! [`plan_catch_up`] bounds the history window to `max_window_ms` behind now
//! and [`catch_up_conversation`] fetches at most `max_pages` pages through the
//! transport's [`HistorySource`], records messages through the inbound log
//! (so they deduplicate against live events) and advances the cursor page by
//! page, stopping on a rate limit. Plan before feeding live events into the
//! cursor, or the gap is skipped.
//!
//! Every function takes `now_ms` from the caller so tests use a fake clock.

use std::future::Future;

use rusqlite::{params, Connection, OptionalExtension, Row, Transaction, TransactionBehavior};

use crate::store::{Store, StoreError, StoreResult};
use crate::surface::{SurfaceAccountRef, SurfaceConversationRef, SurfacePlatform};

/// Additive, idempotent schema for this module. Called from `Store::migrate`.
pub(crate) fn migrate(conn: &Connection) -> StoreResult<()> {
    conn.execute_batch(
        r#"CREATE TABLE IF NOT EXISTS surface_inbound_events (
            seq INTEGER PRIMARY KEY AUTOINCREMENT,
            platform TEXT NOT NULL,
            account_id TEXT NOT NULL,
            conversation_id TEXT NOT NULL,
            thread_id TEXT NOT NULL DEFAULT '',
            event_id TEXT NOT NULL,
            kind TEXT NOT NULL,
            payload TEXT NOT NULL,
            occurred_at_ms INTEGER NOT NULL,
            status TEXT NOT NULL DEFAULT 'received'
                CHECK(status IN ('received', 'claimed', 'handled', 'dead_letter')),
            attempts INTEGER NOT NULL DEFAULT 0,
            last_error TEXT,
            received_at_ms INTEGER NOT NULL,
            claimed_at_ms INTEGER,
            handled_at_ms INTEGER,
            UNIQUE(platform, account_id, event_id)
        );
        CREATE INDEX IF NOT EXISTS idx_surface_inbound_open
        ON surface_inbound_events(platform, account_id, conversation_id, thread_id, status, occurred_at_ms);
        CREATE TABLE IF NOT EXISTS surface_outbox (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            platform TEXT NOT NULL,
            account_id TEXT NOT NULL,
            conversation_id TEXT NOT NULL,
            thread_id TEXT NOT NULL DEFAULT '',
            idempotency_key TEXT NOT NULL,
            operation TEXT NOT NULL
                CHECK(operation IN ('post', 'update', 'upload', 'interaction_response')),
            target_message_id TEXT,
            payload TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'queued'
                CHECK(status IN ('queued', 'sending', 'reconcile', 'sent', 'failed', 'dead_letter', 'abandoned')),
            attempts INTEGER NOT NULL DEFAULT 0,
            max_attempts INTEGER NOT NULL CHECK(max_attempts >= 1),
            next_attempt_at_ms INTEGER NOT NULL,
            interaction_expires_at_ms INTEGER,
            fell_back INTEGER NOT NULL DEFAULT 0 CHECK(fell_back IN (0, 1)),
            provider_message_id TEXT,
            last_error TEXT,
            created_at_ms INTEGER NOT NULL,
            updated_at_ms INTEGER NOT NULL,
            sent_at_ms INTEGER,
            UNIQUE(platform, account_id, idempotency_key)
        );
        CREATE INDEX IF NOT EXISTS idx_surface_outbox_conversation
        ON surface_outbox(platform, account_id, conversation_id, thread_id, status, id);
        CREATE TABLE IF NOT EXISTS surface_cursors (
            platform TEXT NOT NULL,
            account_id TEXT NOT NULL,
            conversation_id TEXT NOT NULL,
            thread_id TEXT NOT NULL DEFAULT '',
            last_message_id TEXT,
            last_seen_at_ms INTEGER NOT NULL,
            updated_at_ms INTEGER NOT NULL,
            PRIMARY KEY(platform, account_id, conversation_id, thread_id)
        );"#,
    )?;
    // #1294: when the last attempt was claimed (lower bound for a provider
    // history lookup) and how many reconcile lookups failed. Additive.
    for (column, ddl) in [
        (
            "last_claimed_at_ms",
            "ALTER TABLE surface_outbox ADD COLUMN last_claimed_at_ms INTEGER",
        ),
        (
            "reconcile_lookups",
            "ALTER TABLE surface_outbox ADD COLUMN reconcile_lookups INTEGER NOT NULL DEFAULT 0",
        ),
    ] {
        let exists: bool = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('surface_outbox') WHERE name = ?1",
            params![column],
            |r| r.get::<_, i64>(0).map(|n| n > 0),
        )?;
        if !exists {
            conn.execute(ddl, [])?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Run `f` in an IMMEDIATE transaction on the store's single connection, so
/// a claim cannot race another process that opened the same file.
fn write_tx<T>(
    store: &Store,
    f: impl FnOnce(&Transaction<'_>) -> StoreResult<T>,
) -> StoreResult<T> {
    store.with_conn(|conn| {
        Ok((|| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
            let out = f(&tx)?;
            tx.commit()?;
            Ok(out)
        })())
    })?
}

fn read<T>(store: &Store, f: impl FnOnce(&Connection) -> StoreResult<T>) -> StoreResult<T> {
    store.with_conn(|conn| Ok(f(conn)))?
}

fn required(value: &str, name: &str) -> StoreResult<()> {
    if value.trim().is_empty() {
        Err(StoreError::InvalidInput(format!("{name} is required")))
    } else {
        Ok(())
    }
}

fn invalid(err: impl std::fmt::Display) -> StoreError {
    StoreError::InvalidInput(err.to_string())
}

struct ConvCols<'a> {
    platform: &'a str,
    account_id: &'a str,
    conversation_id: &'a str,
    thread_id: &'a str,
}

fn cols(conversation: &SurfaceConversationRef) -> ConvCols<'_> {
    ConvCols {
        platform: conversation.account().platform().as_str(),
        account_id: conversation.account().account_id(),
        conversation_id: conversation.conversation_id(),
        thread_id: conversation.thread_id().unwrap_or(""),
    }
}

/// Rebuild a conversation ref from columns `start..start + 4`.
fn conversation_at(
    row: &Row<'_>,
    start: usize,
) -> rusqlite::Result<StoreResult<SurfaceConversationRef>> {
    let platform: String = row.get(start)?;
    let account_id: String = row.get(start + 1)?;
    let conversation_id: String = row.get(start + 2)?;
    let thread_id: String = row.get(start + 3)?;
    Ok((|| {
        let account =
            SurfaceAccountRef::new(SurfacePlatform::new(platform).map_err(invalid)?, account_id)
                .map_err(invalid)?;
        let thread = (!thread_id.is_empty()).then_some(thread_id);
        SurfaceConversationRef::new(account, conversation_id, thread).map_err(invalid)
    })())
}

fn changed_one(changed: usize, what: &str) -> StoreResult<()> {
    if changed == 1 {
        Ok(())
    } else {
        Err(StoreError::InvalidInput(what.to_string()))
    }
}

// ---------------------------------------------------------------------------
// Inbound log
// ---------------------------------------------------------------------------

/// An event the transport has accepted and is about to acknowledge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewInboundEvent {
    pub conversation: SurfaceConversationRef,
    /// Stable provider identity, unique within the platform account.
    pub event_id: String,
    /// `message`, `interaction`, `command`, … — opaque to the store.
    pub kind: String,
    /// Provider time; orders events within a conversation.
    pub occurred_at_ms: i64,
    pub payload: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboundStatus {
    Received,
    Claimed,
    Handled,
    DeadLetter,
}

impl InboundStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Received => "received",
            Self::Claimed => "claimed",
            Self::Handled => "handled",
            Self::DeadLetter => "dead_letter",
        }
    }

    fn parse(value: &str) -> StoreResult<Self> {
        Ok(match value {
            "received" => Self::Received,
            "claimed" => Self::Claimed,
            "handled" => Self::Handled,
            "dead_letter" => Self::DeadLetter,
            other => return Err(invalid(format!("unknown inbound status {other}"))),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboundRecordOutcome {
    /// Newly persisted; safe to acknowledge.
    Accepted { seq: i64 },
    /// Already persisted (redelivery or replay); acknowledge, do not process.
    Duplicate { seq: i64, status: InboundStatus },
}

/// An inbound event handed to the dispatcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedInbound {
    pub seq: i64,
    pub conversation: SurfaceConversationRef,
    pub event_id: String,
    pub kind: String,
    pub occurred_at_ms: i64,
    pub payload: String,
    /// 1 on first delivery; higher values mean an earlier claim was
    /// interrupted or released, so effects may already exist.
    pub attempt: u32,
}

/// What [`Store::recover_surface_delivery`] changed at startup.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    pub inbound_requeued: usize,
    pub sends_to_reconcile: usize,
}

/// Open-row counts for one surface, for `status`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SurfaceDeliveryCounts {
    pub platform: String,
    /// Inbound events received or claimed but not yet handled.
    pub inbound_backlog: i64,
    pub inbound_dead_letter: i64,
    /// Outbound sends not yet settled (queued, sending, failed, reconcile).
    pub outbound_backlog: i64,
    /// Subset of the backlog waiting for a retry after a failure.
    pub outbound_retrying: i64,
    /// Subset of the backlog whose outcome at the provider is unknown.
    pub outbound_reconcile: i64,
    pub outbound_dead_letter: i64,
}

impl Store {
    /// Persist an accepted inbound event. Acknowledge it to the provider only
    /// after this returns `Ok`.
    pub fn record_inbound_event(
        &self,
        event: &NewInboundEvent,
        now_ms: i64,
    ) -> StoreResult<InboundRecordOutcome> {
        required(&event.event_id, "inbound event ID")?;
        required(&event.kind, "inbound event kind")?;
        let c = cols(&event.conversation);
        write_tx(self, |tx| {
            let inserted = tx.execute(
                "INSERT INTO surface_inbound_events
                 (platform, account_id, conversation_id, thread_id, event_id, kind, payload,
                  occurred_at_ms, received_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(platform, account_id, event_id) DO NOTHING",
                params![
                    c.platform,
                    c.account_id,
                    c.conversation_id,
                    c.thread_id,
                    event.event_id,
                    event.kind,
                    event.payload,
                    event.occurred_at_ms,
                    now_ms
                ],
            )?;
            let (seq, status): (i64, String) = tx.query_row(
                "SELECT seq, status FROM surface_inbound_events
                 WHERE platform = ?1 AND account_id = ?2 AND event_id = ?3",
                params![c.platform, c.account_id, event.event_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            Ok(if inserted == 1 {
                InboundRecordOutcome::Accepted { seq }
            } else {
                InboundRecordOutcome::Duplicate {
                    seq,
                    status: InboundStatus::parse(&status)?,
                }
            })
        })
    }

    /// Claim the next event whose conversation is idle, in provider order.
    /// An event already claimed `max_attempts` times is dead-lettered instead.
    pub fn claim_next_inbound_event(
        &self,
        now_ms: i64,
        max_attempts: u32,
    ) -> StoreResult<Option<ClaimedInbound>> {
        self.claim_inbound(None, now_ms, max_attempts)
    }

    /// [`claim_next_inbound_event`](Self::claim_next_inbound_event) limited
    /// to one surface platform, so each surface's dispatcher only ever takes
    /// its own events when several surfaces run in one daemon (#1287).
    pub fn claim_next_inbound_event_for(
        &self,
        platform: &SurfacePlatform,
        now_ms: i64,
        max_attempts: u32,
    ) -> StoreResult<Option<ClaimedInbound>> {
        self.claim_inbound(Some(platform.as_str()), now_ms, max_attempts)
    }

    fn claim_inbound(
        &self,
        platform: Option<&str>,
        now_ms: i64,
        max_attempts: u32,
    ) -> StoreResult<Option<ClaimedInbound>> {
        write_tx(self, |tx| loop {
            let candidate = tx
                .query_row(
                    "SELECT e.seq, e.platform, e.account_id, e.conversation_id, e.thread_id,
                            e.event_id, e.kind, e.occurred_at_ms, e.payload, e.attempts
                     FROM surface_inbound_events e
                     WHERE e.status = 'received'
                       AND (?1 IS NULL OR e.platform = ?1)
                       AND NOT EXISTS (
                         SELECT 1 FROM surface_inbound_events p
                         WHERE p.platform = e.platform AND p.account_id = e.account_id
                           AND p.conversation_id = e.conversation_id AND p.thread_id = e.thread_id
                           AND p.seq != e.seq
                           AND (p.status = 'claimed'
                                OR (p.status = 'received'
                                    AND (p.occurred_at_ms < e.occurred_at_ms
                                         OR (p.occurred_at_ms = e.occurred_at_ms AND p.seq < e.seq)))))
                     ORDER BY e.occurred_at_ms, e.seq
                     LIMIT 1",
                    params![platform],
                    |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            conversation_at(r, 1)?,
                            r.get::<_, String>(5)?,
                            r.get::<_, String>(6)?,
                            r.get::<_, i64>(7)?,
                            r.get::<_, String>(8)?,
                            r.get::<_, u32>(9)?,
                        ))
                    },
                )
                .optional()?;
            let Some((seq, conversation, event_id, kind, occurred_at_ms, payload, attempts)) =
                candidate
            else {
                return Ok(None);
            };
            if attempts >= max_attempts {
                tx.execute(
                    "UPDATE surface_inbound_events SET status = 'dead_letter' WHERE seq = ?1",
                    params![seq],
                )?;
                continue;
            }
            tx.execute(
                "UPDATE surface_inbound_events
                 SET status = 'claimed', attempts = attempts + 1, claimed_at_ms = ?2
                 WHERE seq = ?1",
                params![seq, now_ms],
            )?;
            return Ok(Some(ClaimedInbound {
                seq,
                conversation: conversation?,
                event_id,
                kind,
                occurred_at_ms,
                payload,
                attempt: attempts + 1,
            }));
        })
    }

    /// The dispatcher finished with a claimed event.
    pub fn mark_inbound_handled(&self, seq: i64, now_ms: i64) -> StoreResult<()> {
        write_tx(self, |tx| {
            let changed = tx.execute(
                "UPDATE surface_inbound_events SET status = 'handled', handled_at_ms = ?2
                 WHERE seq = ?1 AND status = 'claimed'",
                params![seq, now_ms],
            )?;
            changed_one(changed, "inbound event is not claimed")
        })
    }

    /// Return a claimed event for another attempt after a handler failure.
    pub fn release_inbound_event(&self, seq: i64, error: &str, _now_ms: i64) -> StoreResult<()> {
        write_tx(self, |tx| {
            let changed = tx.execute(
                "UPDATE surface_inbound_events SET status = 'received', last_error = ?2
                 WHERE seq = ?1 AND status = 'claimed'",
                params![seq, error],
            )?;
            changed_one(changed, "inbound event is not claimed")
        })
    }

    /// Run once at startup, before claiming anything: this process owns the
    /// database, so claims and in-flight sends left behind belong to a
    /// process that died.
    pub fn recover_surface_delivery(&self, now_ms: i64) -> StoreResult<RecoveryReport> {
        write_tx(self, |tx| {
            let inbound_requeued = tx.execute(
                "UPDATE surface_inbound_events SET status = 'received',
                     last_error = COALESCE(last_error, 'interrupted by restart')
                 WHERE status = 'claimed'",
                [],
            )?;
            let sends_to_reconcile = tx.execute(
                "UPDATE surface_outbox SET status = 'reconcile', updated_at_ms = ?1
                 WHERE status = 'sending'",
                params![now_ms],
            )?;
            Ok(RecoveryReport {
                inbound_requeued,
                sends_to_reconcile,
            })
        })
    }

    /// Open-row counts per surface platform, alphabetical. Platforms with no
    /// rows at all are omitted.
    pub fn surface_delivery_counts(&self) -> StoreResult<Vec<SurfaceDeliveryCounts>> {
        read(self, |conn| {
            let mut by_platform =
                std::collections::BTreeMap::<String, SurfaceDeliveryCounts>::new();
            let mut stmt = conn.prepare(
                "SELECT platform,
                        SUM(status IN ('received', 'claimed')),
                        SUM(status = 'dead_letter')
                 FROM surface_inbound_events GROUP BY platform",
            )?;
            let inbound = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for (platform, backlog, dead) in inbound {
                let row =
                    by_platform
                        .entry(platform.clone())
                        .or_insert_with(|| SurfaceDeliveryCounts {
                            platform,
                            ..Default::default()
                        });
                row.inbound_backlog = backlog;
                row.inbound_dead_letter = dead;
            }
            let mut stmt = conn.prepare(
                "SELECT platform,
                        SUM(status IN ('queued', 'sending', 'failed', 'reconcile')),
                        SUM(status = 'failed'),
                        SUM(status = 'reconcile'),
                        SUM(status = 'dead_letter')
                 FROM surface_outbox GROUP BY platform",
            )?;
            let outbound = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, i64>(4)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for (platform, backlog, retrying, reconcile, dead) in outbound {
                let row =
                    by_platform
                        .entry(platform.clone())
                        .or_insert_with(|| SurfaceDeliveryCounts {
                            platform,
                            ..Default::default()
                        });
                row.outbound_backlog = backlog;
                row.outbound_retrying = retrying;
                row.outbound_reconcile = reconcile;
                row.outbound_dead_letter = dead;
            }
            Ok(by_platform.into_values().collect())
        })
    }
}

// ---------------------------------------------------------------------------
// Outbox
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboundOperation {
    /// A new message in the conversation (thread included).
    Post,
    /// Edit of `target_message_id`.
    Update,
    /// A file upload; the payload describes the file.
    Upload,
    /// A reply through a short-lived interaction handle.
    InteractionResponse,
}

impl OutboundOperation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Post => "post",
            Self::Update => "update",
            Self::Upload => "upload",
            Self::InteractionResponse => "interaction_response",
        }
    }

    fn parse(value: &str) -> StoreResult<Self> {
        Ok(match value {
            "post" => Self::Post,
            "update" => Self::Update,
            "upload" => Self::Upload,
            "interaction_response" => Self::InteractionResponse,
            other => return Err(invalid(format!("unknown outbound operation {other}"))),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendStatus {
    Queued,
    Sending,
    /// In flight when the process died; resolve with a [`SendReconciler`]
    /// before any resend.
    Reconcile,
    Sent,
    /// Retryable failure; due again at `next_attempt_at_ms`.
    Failed,
    /// Retries exhausted or a permanent error.
    DeadLetter,
    /// Cancelled on purpose.
    Abandoned,
}

impl SendStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Sending => "sending",
            Self::Reconcile => "reconcile",
            Self::Sent => "sent",
            Self::Failed => "failed",
            Self::DeadLetter => "dead_letter",
            Self::Abandoned => "abandoned",
        }
    }

    fn parse(value: &str) -> StoreResult<Self> {
        Ok(match value {
            "queued" => Self::Queued,
            "sending" => Self::Sending,
            "reconcile" => Self::Reconcile,
            "sent" => Self::Sent,
            "failed" => Self::Failed,
            "dead_letter" => Self::DeadLetter,
            "abandoned" => Self::Abandoned,
            other => return Err(invalid(format!("unknown send status {other}"))),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewOutboundSend {
    pub conversation: SurfaceConversationRef,
    /// Unique per platform account; the transport should also attach it to
    /// the provider message so [`SendReconciler`] can find it.
    pub idempotency_key: String,
    pub operation: OutboundOperation,
    pub target_message_id: Option<String>,
    pub payload: String,
    pub max_attempts: u32,
    /// For [`OutboundOperation::InteractionResponse`]: when the handle dies.
    pub interaction_expires_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueOutcome {
    Queued { id: i64 },
    Duplicate { id: i64, status: SendStatus },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundSend {
    pub id: i64,
    pub conversation: SurfaceConversationRef,
    pub idempotency_key: String,
    pub operation: OutboundOperation,
    pub target_message_id: Option<String>,
    pub payload: String,
    pub status: SendStatus,
    /// Attempts started, including the one being claimed.
    pub attempts: u32,
    pub max_attempts: u32,
    pub next_attempt_at_ms: i64,
    pub interaction_expires_at_ms: Option<i64>,
    /// An expired interaction response rewritten to a normal post.
    pub fell_back: bool,
    pub provider_message_id: Option<String>,
    pub last_error: Option<String>,
    /// When the latest attempt was claimed; a provider lookup for a lost
    /// outcome never needs to look earlier than this.
    pub last_claimed_at_ms: Option<i64>,
    /// Failed reconcile lookups since the send last went to `reconcile`.
    pub reconcile_lookups: u32,
}

/// Exponential backoff: `base * 2^(attempt - 1)`, capped at `max_delay_ms`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub base_delay_ms: i64,
    pub max_delay_ms: i64,
}

impl RetryPolicy {
    pub fn delay_after(&self, attempt: u32) -> i64 {
        let shift = attempt.saturating_sub(1).min(62);
        self.base_delay_ms
            .saturating_mul(1_i64 << shift)
            .min(self.max_delay_ms)
    }
}

/// The provider's answer for a send whose outcome was lost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileOutcome {
    Delivered {
        provider_message_id: String,
    },
    NotDelivered,
    /// The provider could not say; the send stays in `reconcile`.
    Unknown,
}

/// Implemented by each transport: look the send up at the provider (for
/// Slack, recent history matched on the idempotency key in message metadata).
pub trait SendReconciler {
    fn lookup(&self, send: &OutboundSend) -> impl Future<Output = ReconcileOutcome> + Send;
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    pub delivered: usize,
    pub requeued: usize,
    pub dead_lettered: usize,
    pub unknown: usize,
}

const SEND_COLUMNS: &str = "id, platform, account_id, conversation_id, thread_id, idempotency_key,
    operation, target_message_id, payload, status, attempts, max_attempts, next_attempt_at_ms,
    interaction_expires_at_ms, fell_back, provider_message_id, last_error, last_claimed_at_ms,
    reconcile_lookups";

fn send_from_row(r: &Row<'_>) -> rusqlite::Result<StoreResult<OutboundSend>> {
    let conversation = conversation_at(r, 1)?;
    let operation: String = r.get(6)?;
    let status: String = r.get(9)?;
    let id: i64 = r.get(0)?;
    let idempotency_key: String = r.get(5)?;
    let target_message_id: Option<String> = r.get(7)?;
    let payload: String = r.get(8)?;
    let attempts: u32 = r.get(10)?;
    let max_attempts: u32 = r.get(11)?;
    let next_attempt_at_ms: i64 = r.get(12)?;
    let interaction_expires_at_ms: Option<i64> = r.get(13)?;
    let fell_back: bool = r.get::<_, i64>(14)? != 0;
    let provider_message_id: Option<String> = r.get(15)?;
    let last_error: Option<String> = r.get(16)?;
    let last_claimed_at_ms: Option<i64> = r.get(17)?;
    let reconcile_lookups: u32 = r.get(18)?;
    Ok((|| {
        Ok(OutboundSend {
            id,
            conversation: conversation?,
            idempotency_key,
            operation: OutboundOperation::parse(&operation)?,
            target_message_id,
            payload,
            status: SendStatus::parse(&status)?,
            attempts,
            max_attempts,
            next_attempt_at_ms,
            interaction_expires_at_ms,
            fell_back,
            provider_message_id,
            last_error,
            last_claimed_at_ms,
            reconcile_lookups,
        })
    })())
}

fn load_send(conn: &Connection, id: i64) -> StoreResult<Option<OutboundSend>> {
    conn.query_row(
        &format!("SELECT {SEND_COLUMNS} FROM surface_outbox WHERE id = ?1"),
        params![id],
        send_from_row,
    )
    .optional()?
    .transpose()
}

impl Store {
    pub fn enqueue_outbound_send(
        &self,
        send: &NewOutboundSend,
        now_ms: i64,
    ) -> StoreResult<EnqueueOutcome> {
        self.enqueue_outbound_send_at(send, now_ms, now_ms)
    }

    /// [`enqueue_outbound_send`](Self::enqueue_outbound_send), first due at
    /// `due_at_ms` instead of now (#1295: paced notifications). A duplicate
    /// key keeps its existing schedule.
    pub fn enqueue_outbound_send_at(
        &self,
        send: &NewOutboundSend,
        now_ms: i64,
        due_at_ms: i64,
    ) -> StoreResult<EnqueueOutcome> {
        required(&send.idempotency_key, "idempotency key")?;
        if send.max_attempts == 0 {
            return Err(invalid("max_attempts must be at least 1"));
        }
        if send.operation == OutboundOperation::Update && send.target_message_id.is_none() {
            return Err(invalid("an update needs target_message_id"));
        }
        let c = cols(&send.conversation);
        write_tx(self, |tx| {
            let inserted = tx.execute(
                "INSERT INTO surface_outbox
                 (platform, account_id, conversation_id, thread_id, idempotency_key, operation,
                  target_message_id, payload, max_attempts, next_attempt_at_ms,
                  interaction_expires_at_ms, created_at_ms, updated_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?12, ?11, ?10, ?10)
                 ON CONFLICT(platform, account_id, idempotency_key) DO NOTHING",
                params![
                    c.platform,
                    c.account_id,
                    c.conversation_id,
                    c.thread_id,
                    send.idempotency_key,
                    send.operation.as_str(),
                    send.target_message_id,
                    send.payload,
                    send.max_attempts,
                    now_ms,
                    send.interaction_expires_at_ms,
                    due_at_ms
                ],
            )?;
            let (id, status): (i64, String) = tx.query_row(
                "SELECT id, status FROM surface_outbox
                 WHERE platform = ?1 AND account_id = ?2 AND idempotency_key = ?3",
                params![c.platform, c.account_id, send.idempotency_key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            Ok(if inserted == 1 {
                EnqueueOutcome::Queued { id }
            } else {
                EnqueueOutcome::Duplicate {
                    id,
                    status: SendStatus::parse(&status)?,
                }
            })
        })
    }

    /// Claim the next due send whose conversation has nothing earlier still
    /// open. Expired interaction responses are rewritten to a normal post.
    pub fn claim_next_outbound_send(&self, now_ms: i64) -> StoreResult<Option<OutboundSend>> {
        self.claim_outbound(None, now_ms)
    }

    /// [`claim_next_outbound_send`](Self::claim_next_outbound_send) limited
    /// to one platform account, so a transport never claims a send that
    /// belongs to another surface or workspace.
    pub fn claim_next_outbound_send_for(
        &self,
        account: &SurfaceAccountRef,
        now_ms: i64,
    ) -> StoreResult<Option<OutboundSend>> {
        self.claim_outbound(Some(account), now_ms)
    }

    fn claim_outbound(
        &self,
        account: Option<&SurfaceAccountRef>,
        now_ms: i64,
    ) -> StoreResult<Option<OutboundSend>> {
        let platform = account.map(|a| a.platform().as_str());
        let account_id = account.map(|a| a.account_id());
        write_tx(self, |tx| {
            let id: Option<i64> = tx
                .query_row(
                    "SELECT o.id FROM surface_outbox o
                     WHERE o.status IN ('queued', 'failed') AND o.next_attempt_at_ms <= ?1
                       AND (?2 IS NULL OR (o.platform = ?2 AND o.account_id = ?3))
                       AND NOT EXISTS (
                         SELECT 1 FROM surface_outbox p
                         WHERE p.platform = o.platform AND p.account_id = o.account_id
                           AND p.conversation_id = o.conversation_id AND p.thread_id = o.thread_id
                           AND p.id < o.id
                           AND p.status IN ('queued', 'sending', 'failed', 'reconcile'))
                     ORDER BY o.id LIMIT 1",
                    params![now_ms, platform, account_id],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(id) = id else {
                return Ok(None);
            };
            tx.execute(
                "UPDATE surface_outbox
                 SET operation = 'post', fell_back = 1
                 WHERE id = ?1 AND operation = 'interaction_response'
                   AND interaction_expires_at_ms IS NOT NULL AND interaction_expires_at_ms <= ?2",
                params![id, now_ms],
            )?;
            tx.execute(
                "UPDATE surface_outbox
                 SET status = 'sending', attempts = attempts + 1, updated_at_ms = ?2,
                     last_claimed_at_ms = ?2
                 WHERE id = ?1",
                params![id, now_ms],
            )?;
            load_send(tx, id)
        })
    }

    pub fn mark_outbound_sent(
        &self,
        id: i64,
        provider_message_id: &str,
        now_ms: i64,
    ) -> StoreResult<()> {
        required(provider_message_id, "provider message ID")?;
        write_tx(self, |tx| {
            let changed = tx.execute(
                "UPDATE surface_outbox
                 SET status = 'sent', provider_message_id = ?2, sent_at_ms = ?3, updated_at_ms = ?3
                 WHERE id = ?1 AND status = 'sending'",
                params![id, provider_message_id, now_ms],
            )?;
            changed_one(changed, "outbound send is not in flight")
        })
    }

    /// Record a failed attempt. Retryable failures back off until the
    /// attempt budget is spent; permanent ones dead-letter at once.
    pub fn mark_outbound_failed(
        &self,
        id: i64,
        error: &str,
        retryable: bool,
        policy: &RetryPolicy,
        now_ms: i64,
    ) -> StoreResult<SendStatus> {
        write_tx(self, |tx| {
            let Some(send) = load_send(tx, id)? else {
                return Err(invalid("unknown outbound send"));
            };
            if send.status != SendStatus::Sending {
                return Err(invalid("outbound send is not in flight"));
            }
            let (status, next) = if !retryable || send.attempts >= send.max_attempts {
                (SendStatus::DeadLetter, send.next_attempt_at_ms)
            } else {
                (
                    SendStatus::Failed,
                    now_ms + policy.delay_after(send.attempts),
                )
            };
            tx.execute(
                "UPDATE surface_outbox
                 SET status = ?2, last_error = ?3, next_attempt_at_ms = ?4, updated_at_ms = ?5
                 WHERE id = ?1",
                params![id, status.as_str(), error, next, now_ms],
            )?;
            Ok(status)
        })
    }

    /// The attempt may or may not have reached the provider (a timeout or a
    /// dropped connection after the request left). Park the send in
    /// `reconcile` so it is never resent blindly; it keeps holding later
    /// sends in its conversation until [`reconcile_outbound_sends`] or the
    /// owner resolves it.
    pub fn mark_outbound_uncertain(&self, id: i64, error: &str, now_ms: i64) -> StoreResult<()> {
        write_tx(self, |tx| {
            let changed = tx.execute(
                "UPDATE surface_outbox SET status = 'reconcile', last_error = ?2, updated_at_ms = ?3
                 WHERE id = ?1 AND status = 'sending'",
                params![id, error, now_ms],
            )?;
            changed_one(changed, "outbound send is not in flight")
        })
    }

    /// Cancel a send that has not gone out. Not allowed while in flight.
    pub fn abandon_outbound_send(&self, id: i64, reason: &str, now_ms: i64) -> StoreResult<()> {
        write_tx(self, |tx| {
            let changed = tx.execute(
                "UPDATE surface_outbox SET status = 'abandoned', last_error = ?2, updated_at_ms = ?3
                 WHERE id = ?1 AND status IN ('queued', 'failed', 'reconcile')",
                params![id, reason, now_ms],
            )?;
            changed_one(
                changed,
                "outbound send cannot be abandoned in its current state",
            )
        })
    }

    /// Move a send that has not gone out (`queued` or `failed`) to
    /// `next_attempt_at_ms`, optionally replacing its payload. Not an
    /// attempt. `false` when the send is in flight, settled or unknown.
    /// #1295: a notification held through a suspension is re-paced and
    /// marked late before it is sent.
    pub fn reschedule_outbound_send(
        &self,
        id: i64,
        payload: Option<&str>,
        next_attempt_at_ms: i64,
        now_ms: i64,
    ) -> StoreResult<bool> {
        write_tx(self, |tx| {
            let changed = tx.execute(
                "UPDATE surface_outbox
                 SET next_attempt_at_ms = ?2, payload = COALESCE(?3, payload), updated_at_ms = ?4
                 WHERE id = ?1 AND status IN ('queued', 'failed')",
                params![id, next_attempt_at_ms, payload, now_ms],
            )?;
            Ok(changed == 1)
        })
    }

    pub fn outbound_send(&self, id: i64) -> StoreResult<Option<OutboundSend>> {
        read(self, |conn| load_send(conn, id))
    }

    /// Sends whose provider outcome is unknown, oldest first.
    pub fn outbound_sends_awaiting_reconcile(&self) -> StoreResult<Vec<OutboundSend>> {
        read(self, |conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {SEND_COLUMNS} FROM surface_outbox WHERE status = 'reconcile' ORDER BY id"
            ))?;
            let rows = stmt
                .query_map([], send_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows.into_iter().collect()
        })
    }

    /// Sends of one account awaiting reconcile whose next lookup is due
    /// (`next_attempt_at_ms <= now_ms`), oldest first.
    pub fn outbound_sends_awaiting_reconcile_for(
        &self,
        account: &SurfaceAccountRef,
        now_ms: i64,
    ) -> StoreResult<Vec<OutboundSend>> {
        read(self, |conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {SEND_COLUMNS} FROM surface_outbox
                 WHERE status = 'reconcile' AND platform = ?1 AND account_id = ?2
                   AND next_attempt_at_ms <= ?3
                 ORDER BY id"
            ))?;
            let rows = stmt
                .query_map(
                    params![account.platform().as_str(), account.account_id(), now_ms],
                    send_from_row,
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows.into_iter().collect()
        })
    }

    /// A reconcile lookup could not settle the send. It stays in
    /// `reconcile` until `next_lookup_at_ms`. With `count_lookup` the
    /// failure is counted, and once `max_lookups` failures have accrued the
    /// send is dead-lettered so its conversation is no longer held.
    pub fn defer_outbound_reconcile(
        &self,
        id: i64,
        error: &str,
        next_lookup_at_ms: i64,
        count_lookup: bool,
        max_lookups: u32,
        now_ms: i64,
    ) -> StoreResult<SendStatus> {
        write_tx(self, |tx| {
            let Some(send) = load_send(tx, id)? else {
                return Err(invalid("unknown outbound send"));
            };
            if send.status != SendStatus::Reconcile {
                return Err(invalid("outbound send is not awaiting reconcile"));
            }
            let lookups = send.reconcile_lookups + u32::from(count_lookup);
            let status = if count_lookup && lookups >= max_lookups.max(1) {
                SendStatus::DeadLetter
            } else {
                SendStatus::Reconcile
            };
            tx.execute(
                "UPDATE surface_outbox
                 SET status = ?2, last_error = ?3, next_attempt_at_ms = ?4,
                     reconcile_lookups = ?5, updated_at_ms = ?6
                 WHERE id = ?1",
                params![
                    id,
                    status.as_str(),
                    error,
                    next_lookup_at_ms,
                    lookups,
                    now_ms
                ],
            )?;
            Ok(status)
        })
    }

    /// Every send of one account whose idempotency key starts with
    /// `prefix` (compared literally), in enqueue order. An empty `statuses`
    /// means any status.
    pub fn outbound_sends_with_key_prefix(
        &self,
        account: &SurfaceAccountRef,
        prefix: &str,
        statuses: &[SendStatus],
    ) -> StoreResult<Vec<OutboundSend>> {
        // Status names are fixed identifiers from `SendStatus::as_str`.
        let status_filter = if statuses.is_empty() {
            String::new()
        } else {
            let names: Vec<String> = statuses
                .iter()
                .map(|s| format!("'{}'", s.as_str()))
                .collect();
            format!("AND status IN ({})", names.join(", "))
        };
        read(self, |conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {SEND_COLUMNS} FROM surface_outbox
                 WHERE platform = ?1 AND account_id = ?2
                   AND substr(idempotency_key, 1, length(?3)) = ?3
                   {status_filter}
                 ORDER BY id"
            ))?;
            let rows = stmt
                .query_map(
                    params![account.platform().as_str(), account.account_id(), prefix],
                    send_from_row,
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows.into_iter().collect()
        })
    }

    /// Apply a reconcile answer. Returns the send's resulting status.
    pub fn resolve_outbound_reconcile(
        &self,
        id: i64,
        outcome: &ReconcileOutcome,
        now_ms: i64,
    ) -> StoreResult<SendStatus> {
        write_tx(self, |tx| {
            let Some(send) = load_send(tx, id)? else {
                return Err(invalid("unknown outbound send"));
            };
            if send.status != SendStatus::Reconcile {
                return Err(invalid("outbound send is not awaiting reconcile"));
            }
            let status = match outcome {
                ReconcileOutcome::Unknown => return Ok(SendStatus::Reconcile),
                ReconcileOutcome::Delivered {
                    provider_message_id,
                } => {
                    required(provider_message_id, "provider message ID")?;
                    tx.execute(
                        "UPDATE surface_outbox
                         SET status = 'sent', provider_message_id = ?2, sent_at_ms = ?3, updated_at_ms = ?3
                         WHERE id = ?1",
                        params![id, provider_message_id, now_ms],
                    )?;
                    SendStatus::Sent
                }
                ReconcileOutcome::NotDelivered => {
                    let status = if send.attempts >= send.max_attempts {
                        SendStatus::DeadLetter
                    } else {
                        SendStatus::Queued
                    };
                    tx.execute(
                        "UPDATE surface_outbox
                         SET status = ?2, next_attempt_at_ms = ?3, updated_at_ms = ?3,
                             last_error = 'not delivered before restart', reconcile_lookups = 0
                         WHERE id = ?1",
                        params![id, status.as_str(), now_ms],
                    )?;
                    status
                }
            };
            Ok(status)
        })
    }
}

/// Resolve every send left in `reconcile`. Never resends by itself: a send
/// proven absent is requeued for the normal claim loop.
pub async fn reconcile_outbound_sends<R: SendReconciler>(
    store: &Store,
    reconciler: &R,
    now_ms: i64,
) -> StoreResult<ReconcileReport> {
    let mut report = ReconcileReport::default();
    for send in store.outbound_sends_awaiting_reconcile()? {
        let outcome = reconciler.lookup(&send).await;
        match store.resolve_outbound_reconcile(send.id, &outcome, now_ms)? {
            SendStatus::Sent => report.delivered += 1,
            SendStatus::Queued => report.requeued += 1,
            SendStatus::DeadLetter => report.dead_lettered += 1,
            _ => report.unknown += 1,
        }
    }
    Ok(report)
}

// ---------------------------------------------------------------------------
// Gap catch-up
// ---------------------------------------------------------------------------

/// The newest message a conversation is known to have delivered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceCursor {
    pub conversation: SurfaceConversationRef,
    pub last_message_id: Option<String>,
    pub last_seen_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatchUpPolicy {
    /// History older than this behind `now` is not fetched.
    pub max_window_ms: i64,
    pub page_size: u32,
    /// Pages per catch-up run; the next run resumes from the cursor.
    pub max_pages: u32,
}

/// The history window to fetch: `(oldest_ms, latest_ms]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatchUpPlan {
    pub oldest_ms: i64,
    pub latest_ms: i64,
    /// The gap was longer than the window; older messages are not fetched.
    pub truncated: bool,
    pub page_size: u32,
    pub max_pages: u32,
    pub after_message_id: Option<String>,
}

/// `None` when there is nothing to catch up: the conversation was never seen
/// (no backfill of history the owner never had here) or the cursor is current.
pub fn plan_catch_up(
    cursor: Option<&SurfaceCursor>,
    now_ms: i64,
    policy: &CatchUpPolicy,
) -> Option<CatchUpPlan> {
    let cursor = cursor?;
    if cursor.last_seen_at_ms >= now_ms || policy.page_size == 0 || policy.max_pages == 0 {
        return None;
    }
    let floor = now_ms.saturating_sub(policy.max_window_ms.max(0));
    let truncated = cursor.last_seen_at_ms < floor;
    Some(CatchUpPlan {
        oldest_ms: cursor.last_seen_at_ms.max(floor),
        latest_ms: now_ms,
        truncated,
        page_size: policy.page_size,
        max_pages: policy.max_pages,
        after_message_id: (!truncated)
            .then(|| cursor.last_message_id.clone())
            .flatten(),
    })
}

/// One message from provider history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryMessage {
    /// Same identity the live path records, so the two deduplicate.
    pub event_id: String,
    pub message_id: String,
    pub kind: String,
    pub occurred_at_ms: i64,
    pub payload: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryPage {
    /// Messages oldest first; `next` continues the same window.
    Page {
        messages: Vec<HistoryMessage>,
        next: Option<String>,
    },
    RateLimited {
        retry_after_ms: i64,
    },
}

/// Implemented by each transport over its history API.
pub trait HistorySource {
    fn fetch_page(
        &self,
        conversation: &SurfaceConversationRef,
        plan: &CatchUpPlan,
        page_cursor: Option<&str>,
    ) -> impl Future<Output = Result<HistoryPage, String>> + Send;
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatchUpReport {
    pub accepted: usize,
    pub duplicates: usize,
    pub pages: u32,
    pub truncated: bool,
    /// Stopped at the page budget, a rate limit or an error; run again later.
    pub more_pending: bool,
    pub rate_limited_until_ms: Option<i64>,
    pub error: Option<String>,
}

/// Fetch the gap for one conversation within the policy's bounds.
pub async fn catch_up_conversation<H: HistorySource>(
    store: &Store,
    conversation: &SurfaceConversationRef,
    history: &H,
    policy: &CatchUpPolicy,
    now_ms: i64,
) -> StoreResult<CatchUpReport> {
    let cursor = store.surface_cursor(conversation)?;
    let Some(plan) = plan_catch_up(cursor.as_ref(), now_ms, policy) else {
        return Ok(CatchUpReport::default());
    };
    let mut report = CatchUpReport {
        truncated: plan.truncated,
        ..Default::default()
    };
    let mut page_cursor: Option<String> = None;
    loop {
        let page = history
            .fetch_page(conversation, &plan, page_cursor.as_deref())
            .await;
        let (mut messages, next) = match page {
            Err(error) => {
                report.error = Some(error);
                report.more_pending = true;
                break;
            }
            Ok(HistoryPage::RateLimited { retry_after_ms }) => {
                report.rate_limited_until_ms = Some(now_ms + retry_after_ms.max(0));
                report.more_pending = true;
                break;
            }
            Ok(HistoryPage::Page { messages, next }) => (messages, next),
        };
        report.pages += 1;
        // Defensive: never ingest outside the planned window.
        messages
            .retain(|m| m.occurred_at_ms > plan.oldest_ms && m.occurred_at_ms <= plan.latest_ms);
        messages.sort_by_key(|m| m.occurred_at_ms);
        for message in &messages {
            let outcome = store.record_inbound_event(
                &NewInboundEvent {
                    conversation: conversation.clone(),
                    event_id: message.event_id.clone(),
                    kind: message.kind.clone(),
                    occurred_at_ms: message.occurred_at_ms,
                    payload: message.payload.clone(),
                },
                now_ms,
            )?;
            match outcome {
                InboundRecordOutcome::Accepted { .. } => report.accepted += 1,
                InboundRecordOutcome::Duplicate { .. } => report.duplicates += 1,
            }
        }
        if let Some(last) = messages.last() {
            store.advance_surface_cursor(
                conversation,
                &last.message_id,
                last.occurred_at_ms,
                now_ms,
            )?;
        }
        match next {
            None => break,
            Some(_) if report.pages >= plan.max_pages => {
                report.more_pending = true;
                break;
            }
            Some(next) => page_cursor = Some(next),
        }
    }
    Ok(report)
}

impl Store {
    pub fn surface_cursor(
        &self,
        conversation: &SurfaceConversationRef,
    ) -> StoreResult<Option<SurfaceCursor>> {
        let c = cols(conversation);
        read(self, |conn| {
            Ok(conn
                .query_row(
                    "SELECT last_message_id, last_seen_at_ms, updated_at_ms FROM surface_cursors
                     WHERE platform = ?1 AND account_id = ?2 AND conversation_id = ?3 AND thread_id = ?4",
                    params![c.platform, c.account_id, c.conversation_id, c.thread_id],
                    |r| {
                        Ok(SurfaceCursor {
                            conversation: conversation.clone(),
                            last_message_id: r.get(0)?,
                            last_seen_at_ms: r.get(1)?,
                            updated_at_ms: r.get(2)?,
                        })
                    },
                )
                .optional()?)
        })
    }

    /// Move the cursor forward; an older position is ignored.
    pub fn advance_surface_cursor(
        &self,
        conversation: &SurfaceConversationRef,
        message_id: &str,
        seen_at_ms: i64,
        now_ms: i64,
    ) -> StoreResult<()> {
        required(message_id, "surface message ID")?;
        let c = cols(conversation);
        write_tx(self, |tx| {
            tx.execute(
                "INSERT INTO surface_cursors
                 (platform, account_id, conversation_id, thread_id, last_message_id, last_seen_at_ms, updated_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(platform, account_id, conversation_id, thread_id) DO UPDATE SET
                     last_message_id = excluded.last_message_id,
                     last_seen_at_ms = excluded.last_seen_at_ms,
                     updated_at_ms = excluded.updated_at_ms
                 WHERE excluded.last_seen_at_ms > surface_cursors.last_seen_at_ms",
                params![c.platform, c.account_id, c.conversation_id, c.thread_id, message_id, seen_at_ms, now_ms],
            )?;
            Ok(())
        })
    }
}
