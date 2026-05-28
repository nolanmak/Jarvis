//! #219 — observe outbound (SENT) Gmail and supersede stale pending drafts.
//!
//! When the user replies through Gmail web/mobile (or another client) on a
//! thread the daemon has a pending approval card for, two things have to
//! happen:
//!
//! 1. Drop the pending draft. The card is stale (the user already handled
//!    the thread out-of-band) and inflates queue backpressure (#99) until
//!    it's swept.
//! 2. Ingest the reply into wiki/CRM so tone-mirroring (#73) and
//!    commitment-tracking see real user-authored mail.
//!
//! This module ships only step (1) end-to-end. Step (2) is intentionally
//! deferred: hooking the ingest pipeline to outbound mail requires care to
//! avoid inverting commitments ("you wrote to X" vs "X wrote to you") and
//! is a follow-on PR. The `OutboundEvent` struct is the handoff shape that
//! follow-on can subscribe to.
//!
//! Design split:
//!
//! - [`classify_outbound`] is a pure function over a single message + its
//!   headers + the set of known daemon-sent message ids. It's the unit-test
//!   surface for "skip our own sends, emit everything else".
//! - [`OutboundObserver`] wraps a [`GmailApi`] + [`Store`] and runs the
//!   per-tick poll loop: walks every active account, calls
//!   `fetch_sent_history(after: last_seen)`, classifies, persists
//!   `last_seen_sent_at_ms`, calls
//!   `Store::mark_pending_drafts_superseded_by_thread` for each new
//!   outbound event.
//!
//! Default-OFF in the daemon — opt-in via `AUGMENTAGENT_OUTBOUND_OBSERVER=1`.
//! See `serve` wiring in `augmentagent-cli`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tracing::{debug, info, warn};

use augmentagent_store::{Email, Store};

use crate::gmail::GmailApi;

/// The header the daemon sets on every approve-and-send so the observer can
/// recognise its own outbound and skip it. The value is the action id.
///
/// Note: as of #219 the daemon does NOT yet stamp this header on outbound
/// (the existing `send_draft` path goes through Composio's bare
/// `GMAIL_SEND_DRAFT` action which doesn't take custom headers in its
/// current shape). The header check is therefore a best-effort *future*
/// hook: if/when the send path can stamp it, the classifier already honors
/// it. The primary skip path today is message-id matching via
/// `known_self_message_ids`, populated from `actions.messageId` where
/// `status='sent'`.
pub const ACTION_ID_HEADER: &str = "X-AugmentAgent-Action-Id";

/// A newly-observed outbound message that was NOT sent by the daemon —
/// i.e. the user replied out-of-band and we need to react.
///
/// Carries only the minimum the supersede + (future) ingest paths need;
/// the wiki-ingest follow-on can re-fetch full body if it wants more.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundEvent {
    pub message_id: String,
    pub thread_id: Option<String>,
    pub account_entity_id: String,
    pub sent_at_ms: i64,
    /// Short text snippet for log/observability. Not the full body.
    pub snippet: String,
}

/// Outcome of classifying a single outbound message. `Emit` ⇒ user
/// authored it (or we can't prove otherwise); `Skip` ⇒ daemon sent it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Classification {
    Emit(OutboundEvent),
    SkipDaemonSent,
    /// Message has no recoverable id; nothing we can do with it.
    SkipMalformed,
}

/// Pure classifier. Returns `Skip` when:
///
/// - The message carries our `X-AugmentAgent-Action-Id` header (the
///   forward-compatible path; see [`ACTION_ID_HEADER`] doc).
/// - The message id appears in `known_self_message_ids` — the
///   already-`sent` actions table. This is the production skip path today.
///
/// Otherwise emits an `OutboundEvent`. `headers` is keyed lowercased so
/// callers don't have to normalise; an empty map (Composio doesn't surface
/// headers via `GMAIL_FETCH_EMAILS` today) is a fully-supported caller
/// shape — the classifier still works via `known_self_message_ids`.
pub fn classify_outbound(
    email: &Email,
    headers: &HashMap<String, String>,
    known_self_message_ids: &std::collections::HashSet<String>,
) -> Classification {
    if email.message_id.is_empty() {
        return Classification::SkipMalformed;
    }
    // Header-based skip (future-proof; see ACTION_ID_HEADER).
    if headers
        .get(&ACTION_ID_HEADER.to_ascii_lowercase())
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        return Classification::SkipDaemonSent;
    }
    // Production skip: the daemon recorded this message_id as a `sent`
    // action when the approve-button path fired.
    if known_self_message_ids.contains(&email.message_id) {
        return Classification::SkipDaemonSent;
    }
    let sent_at_ms = parse_rfc2822_or_ms(&email.date);
    let snippet = truncate_snippet(&email.body, 160);
    Classification::Emit(OutboundEvent {
        message_id: email.message_id.clone(),
        thread_id: email.thread_id.clone(),
        account_entity_id: email
            .account_entity_id
            .clone()
            .unwrap_or_default(),
        sent_at_ms,
        snippet,
    })
}

/// Best-effort parse of Gmail's `Date:` header. Composio sometimes returns
/// the RFC-2822 form (`Tue, 18 Apr 2026 10:15:00 -0700`), sometimes an
/// internal-date ms-since-epoch number, sometimes ISO-8601. We don't
/// depend on chrono in this crate, so this is intentionally narrow:
/// numeric → parse as ms; everything else → 0 (the caller treats 0 as
/// "unknown" and uses wallclock-now as the cursor advance). Wrong-by-a-few-
/// seconds is harmless: the cursor is `MAX(stored, new)` so a stale value
/// just doesn't advance.
fn parse_rfc2822_or_ms(s: &str) -> i64 {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return 0;
    }
    if let Ok(n) = trimmed.parse::<i64>() {
        // Heuristic: > year 2001 in ms (~1e12) ⇒ already ms. Smaller ⇒
        // seconds-since-epoch; multiply.
        if n > 1_000_000_000_000 {
            return n;
        }
        if n > 1_000_000_000 {
            return n * 1000;
        }
    }
    0
}

fn truncate_snippet(body: &str, max: usize) -> String {
    let trimmed = body.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    let mut out = String::with_capacity(max + 1);
    for (i, ch) in trimmed.chars().enumerate() {
        if i >= max {
            break;
        }
        out.push(ch);
    }
    out.push('…');
    out
}

/// Outbound-mail observer. Single-account per call: the caller fans out
/// across all `get_active_gmail_accounts()` in the serve-loop wrapper.
pub struct OutboundObserver<G: GmailApi> {
    pub store: Arc<Store>,
    pub gmail: Arc<G>,
    /// Page cap per `poll_once` per account. 200 = covers a busy human's
    /// SENT folder between hourly ticks with headroom. Matches the order
    /// of magnitude `fetch_unread` uses for INBOX.
    pub per_account_limit: u32,
}

impl<G: GmailApi> OutboundObserver<G> {
    pub fn new(store: Arc<Store>, gmail: Arc<G>, per_account_limit: u32) -> Self {
        Self {
            store,
            gmail,
            per_account_limit,
        }
    }

    /// Run one poll cycle: walk every active Gmail account, ask Composio
    /// for SENT messages newer than the per-account cursor, classify each,
    /// supersede pending drafts on matching threads, advance the cursor.
    /// Returns the collected `OutboundEvent`s so the follow-on wiki-ingest
    /// hook can fan them out without re-polling Composio.
    pub async fn poll_once(&self) -> anyhow::Result<Vec<OutboundEvent>> {
        let accounts = self.store.get_active_gmail_accounts()?;
        if accounts.is_empty() {
            debug!("outbound observer: no active gmail accounts");
            return Ok(Vec::new());
        }
        let known_self_ids = self.load_self_sent_message_ids()?;
        let mut emitted: Vec<OutboundEvent> = Vec::new();
        for account in accounts {
            let last_seen = self
                .store
                .outbound_last_seen(&account.entity_id)
                .unwrap_or(None)
                .unwrap_or(0);
            // First-tick guard: when `last_seen` is 0 we don't want to
            // ingest the user's entire SENT history. Seed the cursor to
            // wallclock-now and emit nothing this tick; subsequent ticks
            // pick up only genuinely new outbound. The hook for a full
            // backfill is `augmentagent-channel-email::tone::backfill`
            // (an explicit operator command), not the live observer.
            if last_seen == 0 {
                let now_ms = wallclock_ms();
                if let Err(e) = self
                    .store
                    .set_outbound_last_seen(&account.entity_id, now_ms)
                {
                    warn!(
                        account = %account.entity_id,
                        "outbound observer: failed to seed last_seen: {e:#}"
                    );
                }
                debug!(
                    account = %account.entity_id,
                    seeded_at_ms = now_ms,
                    "outbound observer: first tick — seeding cursor, skipping backfill"
                );
                continue;
            }
            // Convert the cursor to Gmail's `after:YYYY/MM/DD` filter. We
            // err generous (one calendar day earlier) and re-dedupe via the
            // numeric cursor, because Gmail's after: granularity is a day,
            // not a second — without the dedup we'd miss messages sent
            // within the cursor day.
            let after_iso = ms_to_after_filter(last_seen);
            let fetched = match self
                .gmail
                .fetch_sent_history(
                    &account.entity_id,
                    Some(&after_iso),
                    self.per_account_limit,
                )
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    warn!(
                        account = %account.entity_id,
                        "outbound observer: fetch_sent_history failed: {e}"
                    );
                    continue;
                }
            };
            if fetched.is_empty() {
                continue;
            }
            let mut newest_seen = last_seen;
            for email in fetched {
                // Composio doesn't surface custom headers via
                // GMAIL_FETCH_EMAILS today; pass an empty map so the
                // classifier falls through to the message-id check.
                let headers: HashMap<String, String> = HashMap::new();
                match classify_outbound(&email, &headers, &known_self_ids) {
                    Classification::Emit(ev) => {
                        if ev.sent_at_ms > newest_seen {
                            newest_seen = ev.sent_at_ms;
                        }
                        // Numeric-cursor dedup: if the Gmail Date header
                        // didn't parse to a usable timestamp (sent_at_ms
                        // == 0), we still emit, but we don't let it pull
                        // the cursor backwards.
                        if ev.sent_at_ms != 0 && ev.sent_at_ms <= last_seen {
                            // Already-seen-but-within-day-granularity;
                            // skip.
                            continue;
                        }
                        if let Some(tid) = &ev.thread_id {
                            match self.store.mark_pending_drafts_superseded_by_thread(
                                tid,
                                "superseded by manual reply",
                            ) {
                                Ok(ids) if !ids.is_empty() => {
                                    info!(
                                        account = %account.entity_id,
                                        thread = %tid,
                                        superseded = ids.len(),
                                        "outbound observer: superseded pending drafts"
                                    );
                                }
                                Ok(_) => {
                                    debug!(
                                        account = %account.entity_id,
                                        thread = %tid,
                                        "outbound observer: no pending drafts on thread"
                                    );
                                }
                                Err(e) => {
                                    warn!(
                                        account = %account.entity_id,
                                        thread = %tid,
                                        "outbound observer: supersede failed: {e:#}"
                                    );
                                }
                            }
                        }
                        emitted.push(ev);
                    }
                    Classification::SkipDaemonSent => {
                        debug!(
                            account = %account.entity_id,
                            message_id = %email.message_id,
                            "outbound observer: skipping daemon-sent message"
                        );
                    }
                    Classification::SkipMalformed => {
                        warn!(
                            account = %account.entity_id,
                            "outbound observer: skipping malformed message (no id)"
                        );
                    }
                }
            }
            if newest_seen > last_seen {
                if let Err(e) = self
                    .store
                    .set_outbound_last_seen(&account.entity_id, newest_seen)
                {
                    warn!(
                        account = %account.entity_id,
                        "outbound observer: failed to persist last_seen: {e:#}"
                    );
                }
            }
        }
        Ok(emitted)
    }

    /// Pull every `actions.messageId` where status='sent' — these are the
    /// message ids the approve-button path produced, so SENT-folder
    /// observations matching them must be skipped. Bounded by recency
    /// (last 30 days) so the set stays small.
    fn load_self_sent_message_ids(&self) -> anyhow::Result<std::collections::HashSet<String>> {
        let cutoff_ms = wallclock_ms() - 30 * 24 * 60 * 60 * 1000;
        let set = self.store.with_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT DISTINCT messageId FROM actions \
                 WHERE status = 'sent' AND createdAt >= ?1",
            )?;
            let rows = stmt.query_map(augmentagent_store::rusqlite::params![cutoff_ms], |r| {
                r.get::<_, String>(0)
            })?;
            let mut out = std::collections::HashSet::new();
            for row in rows {
                out.insert(row?);
            }
            Ok(out)
        })?;
        Ok(set)
    }
}

fn wallclock_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Format `ms` as a `YYYY/MM/DD` string for Gmail's `after:` filter. One
/// calendar day earlier than the literal cursor, so the day-granularity
/// `after:` doesn't miss messages sent inside the cursor day; the numeric
/// cursor in `classify_outbound` handles the dedup.
fn ms_to_after_filter(ms: i64) -> String {
    // Stay chrono-free: hand-roll the day computation. Days-since-epoch =
    // ms / 86_400_000. Subtract one day for safety, then convert to
    // y/m/d via the civil-from-days algorithm (Howard Hinnant).
    let secs = (ms / 1000) - 24 * 60 * 60;
    let days = secs.div_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!("{:04}/{:02}/{:02}", y, m, d)
}

/// Howard Hinnant's `civil_from_days` — days-since-1970-01-01 to (y, m, d).
/// Public-domain algorithm. Used so this crate stays chrono-free.
fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = y + if m <= 2 { 1 } else { 0 };
    (y as i32, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gmail::GmailError;
    use std::collections::HashSet;
    use std::sync::Mutex;
    use async_trait::async_trait;

    fn mk_email(id: &str, thread: Option<&str>, account: &str) -> Email {
        Email {
            message_id: id.to_string(),
            thread_id: thread.map(String::from),
            from: "them@example.com".to_string(),
            subject: "re: thing".to_string(),
            body: "Body text here".to_string(),
            date: "1714000000000".to_string(), // ms
            account_entity_id: Some(account.to_string()),
            platform: "gmail".into(),
            kind: "dm".into(),
        }
    }

    // --- classifier tests (the issue's required unit coverage) ---

    /// Outbound carrying our `X-AugmentAgent-Action-Id` header → skipped.
    /// Forward-compatible: today Composio doesn't expose headers, but
    /// when it does the classifier already honors them.
    #[test]
    fn classify_skips_when_action_id_header_present() {
        let email = mk_email("m1", Some("T1"), "ent-1");
        let mut headers = HashMap::new();
        headers.insert(
            ACTION_ID_HEADER.to_ascii_lowercase(),
            "act-abc".to_string(),
        );
        let known = HashSet::new();
        assert_eq!(
            classify_outbound(&email, &headers, &known),
            Classification::SkipDaemonSent
        );
    }

    /// Outbound matching a recently-`sent` action row → skipped (the
    /// production skip path today, since Composio doesn't pipe headers).
    #[test]
    fn classify_skips_when_message_id_in_known_self_set() {
        let email = mk_email("m1", Some("T1"), "ent-1");
        let headers = HashMap::new();
        let mut known = HashSet::new();
        known.insert("m1".to_string());
        assert_eq!(
            classify_outbound(&email, &headers, &known),
            Classification::SkipDaemonSent
        );
    }

    /// Outbound with no marker and not in the daemon-sent set → emitted.
    /// Covers the user-replied-out-of-band base case from the issue.
    #[test]
    fn classify_emits_user_authored_message() {
        let email = mk_email("m1", Some("T1"), "ent-1");
        let headers = HashMap::new();
        let known = HashSet::new();
        match classify_outbound(&email, &headers, &known) {
            Classification::Emit(ev) => {
                assert_eq!(ev.message_id, "m1");
                assert_eq!(ev.thread_id.as_deref(), Some("T1"));
                assert_eq!(ev.account_entity_id, "ent-1");
                assert_eq!(ev.snippet, "Body text here");
                assert_eq!(ev.sent_at_ms, 1_714_000_000_000);
            }
            other => panic!("expected Emit, got {other:?}"),
        }
    }

    /// Empty message_id ⇒ SkipMalformed (the classifier never emits a
    /// useless event).
    #[test]
    fn classify_skips_when_message_id_empty() {
        let mut email = mk_email("m1", Some("T1"), "ent-1");
        email.message_id = String::new();
        let headers = HashMap::new();
        let known = HashSet::new();
        assert_eq!(
            classify_outbound(&email, &headers, &known),
            Classification::SkipMalformed
        );
    }

    /// Empty header value must NOT be treated as "marker present".
    #[test]
    fn classify_ignores_empty_header_value() {
        let email = mk_email("m1", Some("T1"), "ent-1");
        let mut headers = HashMap::new();
        headers.insert(ACTION_ID_HEADER.to_ascii_lowercase(), String::new());
        let known = HashSet::new();
        match classify_outbound(&email, &headers, &known) {
            Classification::Emit(_) => {}
            other => panic!("expected Emit on empty header value, got {other:?}"),
        }
    }

    // --- snippet + date-parse edge cases ---

    #[test]
    fn snippet_truncates_with_ellipsis() {
        let long = "a".repeat(200);
        assert_eq!(truncate_snippet(&long, 10), format!("{}…", "a".repeat(10)));
    }

    #[test]
    fn snippet_keeps_short_body_intact() {
        assert_eq!(truncate_snippet("short", 10), "short");
    }

    #[test]
    fn parse_date_accepts_ms_and_seconds() {
        assert_eq!(parse_rfc2822_or_ms("1714000000000"), 1_714_000_000_000);
        assert_eq!(parse_rfc2822_or_ms("1714000000"), 1_714_000_000_000);
        assert_eq!(parse_rfc2822_or_ms("garbage"), 0);
        assert_eq!(parse_rfc2822_or_ms(""), 0);
    }

    #[test]
    fn after_filter_yields_iso_dayslash_format() {
        // Epoch + 1 day ⇒ filter is for 1970/01/01 (we step one day back).
        let s = ms_to_after_filter(2 * 86_400_000);
        assert_eq!(s.len(), 10);
        assert!(s.contains('/'));
    }

    // --- OutboundObserver integration test with a fake GmailApi ---

    struct FakeGmail {
        sent_by_entity: Mutex<HashMap<String, Vec<Email>>>,
    }

    impl FakeGmail {
        fn new() -> Self {
            Self {
                sent_by_entity: Mutex::new(HashMap::new()),
            }
        }
    }

    #[async_trait]
    impl GmailApi for FakeGmail {
        async fn fetch_unread(
            &self,
            _entity_id: &str,
            _limit: u32,
        ) -> Result<Vec<Email>, GmailError> {
            Ok(Vec::new())
        }

        async fn fetch_with_query(
            &self,
            _entity_id: &str,
            _query: &str,
            _limit: u32,
        ) -> Result<Vec<Email>, GmailError> {
            Ok(Vec::new())
        }

        async fn fetch_sent_history(
            &self,
            entity_id: &str,
            _since_iso: Option<&str>,
            _max_total: u32,
        ) -> Result<Vec<Email>, GmailError> {
            Ok(self
                .sent_by_entity
                .lock()
                .unwrap()
                .get(entity_id)
                .cloned()
                .unwrap_or_default())
        }

        async fn create_draft(
            &self,
            _e: &str,
            _t: &str,
            _s: &str,
            _b: &str,
            _th: Option<&str>,
        ) -> Result<String, GmailError> {
            Ok("draft".into())
        }

        async fn update_draft(
            &self,
            _e: &str,
            _d: &str,
            _t: &str,
            _s: &str,
            _b: &str,
        ) -> Result<(), GmailError> {
            Ok(())
        }

        async fn send_draft(&self, _e: &str, _d: &str) -> Result<(), GmailError> {
            Ok(())
        }

        async fn delete_draft(&self, _e: &str, _d: &str) -> Result<(), GmailError> {
            Ok(())
        }
    }

    /// Build a Store with the columns the observer touches. Mirrors
    /// `inbound::tests::mk_store` so the two stay in lockstep.
    fn mk_store(entities: &[&str]) -> (Arc<Store>, tempfile::NamedTempFile) {
        use augmentagent_store::rusqlite::Connection;
        let file = tempfile::NamedTempFile::new().unwrap();
        {
            let conn = Connection::open(file.path()).unwrap();
            conn.execute_batch(
                r#"
                CREATE TABLE actions (
                    id TEXT PRIMARY KEY, messageId TEXT NOT NULL, threadId TEXT,
                    fromEmail TEXT NOT NULL, subject TEXT NOT NULL,
                    originalBody TEXT, draftBody TEXT,
                    status TEXT NOT NULL DEFAULT 'pending', errorMessage TEXT,
                    createdAt INTEGER NOT NULL, updatedAt INTEGER NOT NULL
                );
                CREATE TABLE emails (
                    messageId TEXT PRIMARY KEY, threadId TEXT,
                    fromEmail TEXT NOT NULL, subject TEXT NOT NULL,
                    body TEXT, receivedAt TEXT, accountEntityId TEXT,
                    firstSeenAt INTEGER NOT NULL, triageResult TEXT, agentProcessedAt INTEGER
                );
                CREATE TABLE gmail_accounts (
                    id TEXT PRIMARY KEY, connectionId TEXT NOT NULL, email TEXT,
                    label TEXT, entityId TEXT NOT NULL, active INTEGER DEFAULT 1,
                    createdAt INTEGER NOT NULL
                );
                "#,
            )
            .unwrap();
            for (i, ent) in entities.iter().enumerate() {
                conn.execute(
                    "INSERT INTO gmail_accounts VALUES (?1, ?2, ?3, NULL, ?4, 1, 0)",
                    augmentagent_store::rusqlite::params![
                        format!("a{i}"),
                        format!("c{i}"),
                        format!("{ent}@x.com"),
                        ent
                    ],
                )
                .unwrap();
            }
        }
        (Arc::new(Store::open(file.path()).unwrap()), file)
    }

    /// First tick on a fresh cursor seeds last_seen without emitting
    /// events — we must NOT backfill the entire SENT folder.
    #[tokio::test]
    async fn poll_once_first_tick_seeds_cursor_without_backfill() {
        let (store, _f) = mk_store(&["ent-1"]);
        let gmail = Arc::new(FakeGmail::new());
        gmail
            .sent_by_entity
            .lock()
            .unwrap()
            .insert("ent-1".into(), vec![mk_email("m1", Some("T1"), "ent-1")]);

        let observer = OutboundObserver::new(Arc::clone(&store), gmail, 50);
        let events = observer.poll_once().await.unwrap();
        assert!(
            events.is_empty(),
            "first tick must not emit; got {events:?}"
        );
        assert!(
            store.outbound_last_seen("ent-1").unwrap().unwrap_or(0) > 0,
            "first tick must seed the cursor"
        );
    }

    /// Second tick: cursor is set, fake returns one outbound message that
    /// is NOT in the daemon-sent set — observer must emit one event AND
    /// supersede pending drafts on its thread.
    #[tokio::test]
    async fn poll_once_emits_and_supersedes_on_user_reply() {
        let (store, _f) = mk_store(&["ent-1"]);
        // Seed the cursor (skip the first-tick guard).
        store.set_outbound_last_seen("ent-1", 1_000_000).unwrap();
        // Seed a pending action on thread T1 — the observer should flip it.
        let pending_id = store
            .log_action(
                "in-msg-1",
                Some("T1"),
                "alice@example.com",
                "subj",
                None,
                Some("draft body"),
                augmentagent_store::ActionStatus::Pending,
            )
            .unwrap();

        let gmail = Arc::new(FakeGmail::new());
        gmail.sent_by_entity.lock().unwrap().insert(
            "ent-1".into(),
            vec![mk_email("sent-msg-1", Some("T1"), "ent-1")],
        );

        let observer = OutboundObserver::new(Arc::clone(&store), gmail, 50);
        let events = observer.poll_once().await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].message_id, "sent-msg-1");
        assert_eq!(events[0].thread_id.as_deref(), Some("T1"));

        // Pending draft on T1 must be superseded.
        let status = store
            .with_conn(|c| {
                c.query_row(
                    "SELECT status FROM actions WHERE id = ?1",
                    augmentagent_store::rusqlite::params![pending_id],
                    |r| r.get::<_, String>(0),
                )
            })
            .unwrap();
        assert_eq!(status, "superseded");
    }

    /// Daemon-sent outbound (message_id matches an `actions` row with
    /// status='sent') must be skipped — and the pending draft on that
    /// thread, if any, must NOT be touched.
    #[tokio::test]
    async fn poll_once_skips_daemon_sent_via_known_message_id() {
        let (store, _f) = mk_store(&["ent-1"]);
        store.set_outbound_last_seen("ent-1", 1_000_000).unwrap();
        // A `sent` action sharing the same message_id the fake returns.
        store
            .log_action(
                "sent-msg-1",
                Some("T1"),
                "alice@example.com",
                "subj",
                None,
                Some("draft body"),
                augmentagent_store::ActionStatus::Sent,
            )
            .unwrap();
        // Separately, an unrelated pending draft on T1 (e.g. a second
        // person on the same thread) — must NOT be touched, the daemon
        // already handled what we just observed.
        let unrelated_pending = store
            .log_action(
                "in-msg-1",
                Some("T1"),
                "alice@example.com",
                "subj",
                None,
                Some("draft body"),
                augmentagent_store::ActionStatus::Pending,
            )
            .unwrap();

        let gmail = Arc::new(FakeGmail::new());
        gmail.sent_by_entity.lock().unwrap().insert(
            "ent-1".into(),
            vec![mk_email("sent-msg-1", Some("T1"), "ent-1")],
        );

        let observer = OutboundObserver::new(Arc::clone(&store), gmail, 50);
        let events = observer.poll_once().await.unwrap();
        assert!(events.is_empty(), "daemon-sent must be skipped: {events:?}");

        let status = store
            .with_conn(|c| {
                c.query_row(
                    "SELECT status FROM actions WHERE id = ?1",
                    augmentagent_store::rusqlite::params![unrelated_pending],
                    |r| r.get::<_, String>(0),
                )
            })
            .unwrap();
        assert_eq!(
            status, "pending",
            "skipping daemon-sent must NOT touch other pending drafts on the thread"
        );
    }

    /// No active accounts ⇒ empty Vec, no error.
    #[tokio::test]
    async fn poll_once_empty_when_no_accounts() {
        let (store, _f) = mk_store(&[]);
        let gmail = Arc::new(FakeGmail::new());
        let observer = OutboundObserver::new(store, gmail, 50);
        let events = observer.poll_once().await.unwrap();
        assert!(events.is_empty());
    }
}
