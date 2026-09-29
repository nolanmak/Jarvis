//! #1295 — proactive notifications on Slack.
//!
//! Digests, reminders, audit and health notices, review and background
//! results go to the owner's DM or the bound control channel through the
//! durable outbox, the same one answers use, under
//! `turn:notify:<class>:<dedupe key>:text:<n>`. So:
//!
//! * a producer that runs twice (a retry after another surface failed, a
//!   restart) queues nothing new: the key is already there;
//! * a burst is paced: each send is first due `spacing_ms` after the
//!   previous pending notification, so a backlog never meets Slack's
//!   per-channel rate limit at once;
//! * a notification that is delivered more than `late_after_ms` after it
//!   was due (the Mac slept, the daemon was stopped) says so. A producer
//!   that fires late marks it when it queues it; a send that waited in the
//!   outbox is marked by [`catch_up_notifications`], which the dispatcher
//!   runs at the start of every drain, and re-paced from the wake time, so
//!   a backlog of `n` goes out within `(n - 1) * spacing_ms` of waking.
//!
//! The pacing and threshold travel in each send's payload (`notify`), so
//! the dispatcher needs no configuration of its own.

use std::sync::Arc;

use augmentagent_store::delivery::{EnqueueOutcome, SendStatus};
use augmentagent_store::{Store, StoreResult, SurfaceAccountRef, SurfaceConversationRef};
use serde_json::Value;

use crate::delivery::{plan_answer, Answer, PlanError, PlanOptions};

/// Outbox key prefix of every notification send.
pub const NOTIFY_KEY_PREFIX: &str = "turn:notify:";

/// How every late marker begins.
pub const LATE_MARKER_PREFIX: &str = ":alarm_clock: _Late:";

/// `notify:<class>:<dedupe key>`, the turn ID the outbox keys derive from.
pub fn notify_turn_id(class: &str, dedupe_key: &str) -> String {
    format!("notify:{class}:{dedupe_key}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotifyPacing {
    /// Delivered later than this after it was due → marked late.
    pub late_after_ms: i64,
    /// Minimum gap between two notification sends.
    pub spacing_ms: i64,
}

impl Default for NotifyPacing {
    fn default() -> Self {
        Self {
            late_after_ms: 5 * 60_000,
            // Slack allows about one message per second per channel.
            spacing_ms: 1_100,
        }
    }
}

/// One notification.
#[derive(Debug, Clone, Copy)]
pub struct SlackNotification<'a> {
    /// Notification class (`digest`, `reminder`, `audit`, ...).
    pub class: &'a str,
    /// Stable per logical notification; the same key never posts twice.
    pub dedupe_key: &'a str,
    pub markdown: &'a str,
    /// When the owner should have seen it.
    pub due_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotifyEnqueued {
    pub turn_id: String,
    pub queued: usize,
    /// Already queued or sent under this key; nothing new was queued.
    pub duplicate: bool,
    /// Marked late when queued.
    pub late: bool,
    /// When the first part is due.
    pub first_due_at_ms: i64,
}

/// Queues notifications for one Slack conversation (the owner's DM or
/// control channel).
pub struct SlackNotifier {
    store: Arc<Store>,
    destination: SurfaceConversationRef,
    pacing: NotifyPacing,
    plan: PlanOptions,
}

impl SlackNotifier {
    pub fn new(store: Arc<Store>, destination: SurfaceConversationRef) -> Self {
        Self {
            store,
            destination,
            pacing: NotifyPacing::default(),
            plan: PlanOptions::default(),
        }
    }

    pub fn with_pacing(mut self, pacing: NotifyPacing) -> Self {
        self.pacing = pacing;
        self
    }

    pub fn destination(&self) -> &SurfaceConversationRef {
        &self.destination
    }

    /// Queue `n` for the destination. Safe to call again: a known key is a
    /// duplicate and queues nothing.
    pub fn enqueue(
        &self,
        n: &SlackNotification<'_>,
        now_ms: i64,
    ) -> Result<NotifyEnqueued, PlanError> {
        let turn_id = notify_turn_id(n.class, n.dedupe_key);
        let sends = plan_answer(
            &self.destination,
            &Answer {
                turn_id: &turn_id,
                markdown: n.markdown,
                files: &[],
            },
            &self.plan,
        )?;
        let late = now_ms.saturating_sub(n.due_at_ms) > self.pacing.late_after_ms;
        let meta = Meta {
            due_at_ms: n.due_at_ms,
            late_after_ms: self.pacing.late_after_ms,
            spacing_ms: self.pacing.spacing_ms,
            late,
        };
        let account = &self.destination.account();
        let mut slot = next_free_slot(&self.store, account, now_ms, self.pacing.spacing_ms)?;
        let mut out = NotifyEnqueued {
            turn_id: turn_id.clone(),
            queued: 0,
            duplicate: false,
            late,
            first_due_at_ms: slot,
        };
        for (index, mut send) in sends.into_iter().enumerate() {
            let mut payload: Value = serde_json::from_str(&send.payload)
                .map_err(|e| PlanError::Invalid(e.to_string()))?;
            if late && index == 0 {
                prefix_text(&mut payload, &late_marker(n.due_at_ms, now_ms));
            }
            payload["notify"] = meta.to_json();
            send.payload = payload.to_string();
            match self.store.enqueue_outbound_send_at(&send, now_ms, slot)? {
                EnqueueOutcome::Queued { .. } => {
                    if out.queued == 0 {
                        out.first_due_at_ms = slot;
                    }
                    out.queued += 1;
                    slot += self.pacing.spacing_ms;
                }
                EnqueueOutcome::Duplicate { .. } => {}
            }
        }
        out.duplicate = out.queued == 0;
        Ok(out)
    }
}

/// Pacing and lateness carried in each notification send's payload.
#[derive(Debug, Clone, Copy)]
struct Meta {
    due_at_ms: i64,
    late_after_ms: i64,
    spacing_ms: i64,
    late: bool,
}

impl Meta {
    fn to_json(self) -> Value {
        serde_json::json!({
            "due_at_ms": self.due_at_ms,
            "late_after_ms": self.late_after_ms,
            "spacing_ms": self.spacing_ms,
            "late": self.late,
        })
    }

    fn from_payload(payload: &Value) -> Option<Self> {
        let m = payload.get("notify")?;
        Some(Self {
            due_at_ms: m.get("due_at_ms")?.as_i64()?,
            late_after_ms: m.get("late_after_ms")?.as_i64()?,
            spacing_ms: m.get("spacing_ms")?.as_i64()?.max(0),
            late: m.get("late")?.as_bool()?,
        })
    }
}

const PENDING: &[SendStatus] = &[SendStatus::Queued, SendStatus::Failed];

/// `now`, or one spacing after the latest pending notification send.
fn next_free_slot(
    store: &Store,
    account: &SurfaceAccountRef,
    now_ms: i64,
    spacing_ms: i64,
) -> StoreResult<i64> {
    let latest = store
        .outbound_sends_with_key_prefix(account, NOTIFY_KEY_PREFIX, PENDING)?
        .iter()
        .map(|s| s.next_attempt_at_ms)
        .max();
    Ok(latest.map_or(now_ms, |l| now_ms.max(l + spacing_ms)))
}

fn prefix_text(payload: &mut Value, marker: &str) {
    if let Some(text) = payload.get("text").and_then(Value::as_str) {
        let marked = format!("{marker}\n{text}");
        payload["text"] = Value::String(marked);
    }
}

/// What [`catch_up_notifications`] changed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CatchUp {
    /// Notifications marked late (one per notification, not per part).
    pub marked_late: usize,
    /// Sends whose next attempt moved.
    pub repaced: usize,
}

/// Mark late and re-pace this account's notification sends that are due
/// but waited past their threshold (the host slept or the daemon was
/// stopped), and keep every pending notification one spacing apart with
/// at most one due at `now_ms`, so a backlog goes out one at a time. Idempotent: a send is
/// marked once, and a paced queue is left as it is.
pub fn catch_up_notifications(
    store: &Store,
    account: &SurfaceAccountRef,
    now_ms: i64,
) -> StoreResult<CatchUp> {
    let mut out = CatchUp::default();
    let mut cursor: Option<i64> = None;
    for send in store.outbound_sends_with_key_prefix(account, NOTIFY_KEY_PREFIX, PENDING)? {
        let Ok(mut payload) = serde_json::from_str::<Value>(&send.payload) else {
            continue;
        };
        let Some(meta) = Meta::from_payload(&payload) else {
            continue;
        };
        let held = send.next_attempt_at_ms <= now_ms
            && !meta.late
            && now_ms.saturating_sub(meta.due_at_ms) > meta.late_after_ms;
        let due = send.next_attempt_at_ms <= now_ms;
        // Only the first due send goes now; every later one is at least one
        // spacing after the one before it, counted from now, so a drain that
        // comes late (the sender polls) never sends a backlog at once.
        let slot = match cursor {
            None if held => now_ms,
            None => send.next_attempt_at_ms,
            Some(c) => send.next_attempt_at_ms.max(now_ms).max(c + meta.spacing_ms),
        };
        cursor = Some(if due { slot.max(now_ms) } else { slot });
        let mut rewritten = None;
        if held {
            if payload.get("part").and_then(Value::as_u64) == Some(1) {
                prefix_text(&mut payload, &late_marker(meta.due_at_ms, now_ms));
                out.marked_late += 1;
            }
            payload["notify"] = Meta { late: true, ..meta }.to_json();
            rewritten = Some(payload.to_string());
        }
        let moved = slot != send.next_attempt_at_ms;
        if rewritten.is_some() || moved {
            let changed =
                store.reschedule_outbound_send(send.id, rewritten.as_deref(), slot, now_ms)?;
            if changed && moved {
                out.repaced += 1;
            }
        }
    }
    Ok(out)
}

/// The line put before a late notification: when it was due (rendered by
/// Slack in the reader's timezone, UTC as the fallback) and how late it is.
pub fn late_marker(due_at_ms: i64, now_ms: i64) -> String {
    let secs = due_at_ms.div_euclid(1000);
    let late_secs = now_ms.saturating_sub(due_at_ms).max(0) / 1000;
    let late_min = late_secs / 60;
    let late = if late_min >= 60 {
        format!("{}h {}m", late_min / 60, late_min % 60)
    } else if late_min >= 1 {
        format!("{late_min}m")
    } else {
        format!("{late_secs}s")
    };
    format!(
        "{LATE_MARKER_PREFIX} due <!date^{secs}^{{date_short_pretty}} {{time}}|{}>, {late} late \
         (this computer was asleep or the daemon was stopped)._",
        utc_minute(secs)
    )
}

/// `YYYY-MM-DD HH:MM UTC` without a date library.
fn utc_minute(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        rem / 3_600,
        (rem % 3_600) / 60
    )
}
