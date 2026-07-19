//! `CalendarChannel` — 15-min hot ticker over the user's primary calendar.
//!
//! Phase 1 cut (archived AugmentAgent#82 §12):
//! - Hot ticker only (no nightly sweep).
//! - Window: now-1h .. now+24h.
//! - Single calendar (`primary`).
//! - One Meeting log line per event instance — no recurrence collapse.
//! - Per-attendee fan-out into the wiki ingest pipeline; never produces a
//!   draft/approval card.
//!
//! Dedup substrate: the `emails` table. We synthesise a row per event,
//! `messageId = "<event_id>"`, `platform = "gcal"`, `kind = "meeting"`. The
//! same row blocks subsequent polls from re-firing ingest for an event we've
//! already processed, mirroring the gmail channel's `is_email_complete` gate.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use augmentagent_channel_core::decision::DecisionKind;
use augmentagent_channel_core::ingest::{spawn_ingest, IngestTrigger};
use augmentagent_channel_core::Reasoner;
use augmentagent_store::{Email, Store, TriageResult};

use crate::alerts::{self, AlertCandidate, AlertSink};
use crate::filter::passes_filter;
use crate::gcal::{CalendarApi, CalendarError};
use crate::trigger::{HOT_LOOKAHEAD_HOURS, HOT_LOOKBACK_HOURS};
use crate::types::{render_meeting_body, truncate, CalendarEvent, MeetingPayload};
use crate::PLATFORM;

#[derive(Clone, Debug)]
pub struct CalendarChannelConfig {
    /// 15 min in production. Overridden in tests for deterministic ticks.
    pub poll_interval: Duration,
    /// `primary` in Phase 1.
    pub calendar_id: String,
    /// Wiki root. `None` = no wiki integration; the channel still records
    /// dedup rows but skips fan-out.
    pub wiki_root: Option<PathBuf>,
    /// Path to the `wiki-skill.md` schema. Required when `wiki_root` is set.
    pub wiki_schema_path: Option<PathBuf>,
    /// Dry-run mode prints the planned WorkItems and synthetic emails to
    /// stdout but does NOT spawn ingest tasks or upsert dedup rows.
    pub dry_run: bool,
    /// Minutes before start inside which an upcoming-meeting alert fires
    /// (#396). Must comfortably exceed the poll cadence or an event can
    /// slip through between two polls.
    pub alert_lead_min: i64,
    /// Local hour (0-23) at/after which the once-per-day agenda posts
    /// (#396). `None` disables the agenda.
    pub agenda_local_hour: Option<u32>,
}

impl Default for CalendarChannelConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(15 * 60),
            calendar_id: "primary".into(),
            wiki_root: None,
            wiki_schema_path: None,
            dry_run: true,
            alert_lead_min: 60,
            agenda_local_hour: Some(8),
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct PollOutcome {
    pub accounts_polled: usize,
    pub events_seen: usize,
    pub events_filtered: usize,
    pub new_events: usize,
    pub fanouts: usize,
    pub forbidden_accounts: usize,
    pub errors: usize,
    /// Alerts delivered this poll (upcoming + conflict + agenda). In
    /// dry-run this counts would-send alerts.
    pub alerts_sent: usize,
}

pub struct CalendarChannel<C: CalendarApi, R: Reasoner> {
    pub store: Arc<Store>,
    pub gcal: Arc<C>,
    pub reasoner: Arc<R>,
    pub config: CalendarChannelConfig,
    /// Loaded wiki schema (`wiki-skill.md`). `None` disables ingest.
    wiki_schema: Option<String>,
    /// Per-account "we already warned about 403" cache so we log the
    /// re-consent message at most once per process per account.
    forbidden_warned: tokio::sync::Mutex<std::collections::HashSet<String>>,
    /// Operator alert transport (#396/#397). `None` disables live alert
    /// delivery; dry-run still prints would-send lines.
    alert_sink: Option<Arc<dyn AlertSink>>,
}

impl<C: CalendarApi, R: Reasoner + 'static> CalendarChannel<C, R> {
    pub fn new(
        store: Arc<Store>,
        gcal: Arc<C>,
        reasoner: Arc<R>,
        config: CalendarChannelConfig,
    ) -> Self {
        let wiki_schema = match (&config.wiki_root, &config.wiki_schema_path) {
            (Some(root), Some(schema_path)) => {
                let layout = augmentagent_wiki::WikiLayout::new(root.clone());
                if let Err(e) = layout.bootstrap() {
                    warn!("wiki bootstrap failed, disabling wiki: {e}");
                    None
                } else {
                    match std::fs::read_to_string(schema_path) {
                        Ok(s) if !s.trim().is_empty() => {
                            info!(path = %schema_path.display(), "calendar wiki schema loaded");
                            Some(s)
                        }
                        Ok(_) => {
                            warn!("calendar wiki schema is empty; disabling wiki");
                            None
                        }
                        Err(e) => {
                            warn!("calendar wiki schema read failed, disabling wiki: {e}");
                            None
                        }
                    }
                }
            }
            _ => None,
        };
        Self {
            store,
            gcal,
            reasoner,
            config,
            wiki_schema,
            forbidden_warned: tokio::sync::Mutex::new(Default::default()),
            alert_sink: None,
        }
    }

    /// Attach an operator alert transport (#396/#397). Without one, live
    /// polls skip the alert pass entirely.
    pub fn with_alert_sink(mut self, sink: Arc<dyn AlertSink>) -> Self {
        self.alert_sink = Some(sink);
        self
    }

    pub async fn run(&self, shutdown: CancellationToken) -> anyhow::Result<()> {
        let mut hot = tokio::time::interval(self.config.poll_interval);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("calendar channel: shutdown signal received");
                    return Ok(());
                }
                _ = hot.tick() => {
                    match self.poll_once().await {
                        Ok(outcome) => info!(?outcome, "calendar poll complete"),
                        Err(e) => error!("calendar poll failed: {e:#}"),
                    }
                }
            }
        }
    }

    pub async fn poll_once(&self) -> anyhow::Result<PollOutcome> {
        let mut outcome = PollOutcome::default();
        let accounts = self.store.get_active_gmail_accounts()?;
        outcome.accounts_polled = accounts.len();
        if accounts.is_empty() {
            warn!("no active gmail accounts; calendar polling skipped");
            return Ok(outcome);
        }

        let now = Utc::now();
        let time_min = now - chrono::Duration::hours(HOT_LOOKBACK_HOURS);
        let time_max = now + chrono::Duration::hours(HOT_LOOKAHEAD_HOURS);

        for account in accounts {
            match self
                .gcal
                .list_events(
                    &account.entity_id,
                    &self.config.calendar_id,
                    time_min,
                    time_max,
                )
                .await
            {
                Ok(events) => {
                    outcome.events_seen += events.len();
                    outcome.alerts_sent += self
                        .run_alert_pass(&events, &account.entity_id, now)
                        .await;
                    for ev in events {
                        if let Err(reason) = passes_filter(&ev) {
                            outcome.events_filtered += 1;
                            debug!(
                                event_id = %ev.id,
                                skip = reason.label(),
                                "calendar event filtered"
                            );
                            continue;
                        }
                        let Some(payload) = MeetingPayload::from_event(
                            &ev,
                            &account.entity_id,
                            &self.config.calendar_id,
                        ) else {
                            debug!(event_id = %ev.id, "missing start/end; skipping");
                            continue;
                        };

                        // Dedup gate: have we already processed this event?
                        if self.store.is_email_complete(&payload.event_id)? {
                            continue;
                        }

                        let fanouts = self
                            .process_event(&payload, &account.email)
                            .await?;
                        outcome.new_events += 1;
                        outcome.fanouts += fanouts;
                    }
                }
                Err(CalendarError::Forbidden { message }) => {
                    let mut warned = self.forbidden_warned.lock().await;
                    if warned.insert(account.entity_id.clone()) {
                        warn!(
                            account = %account.entity_id,
                            "calendar 403 — re-consent calendar.readonly scope: {message}"
                        );
                    }
                    outcome.forbidden_accounts += 1;
                }
                Err(e) => {
                    outcome.errors += 1;
                    error!(account = %account.entity_id, "list_events failed: {e:#}");
                }
            }
        }
        Ok(outcome)
    }

    /// #396/#397 — imminent-start, conflict, and daily-agenda alerts over
    /// the freshly fetched hot window. Detection is in [`crate::alerts`];
    /// dedup rides the `calendar_alerts` store table so re-polls stay
    /// silent and reschedules re-arm. Returns alerts delivered (or that
    /// would deliver, in dry-run). Never fails the poll: store/transport
    /// errors are logged and swallowed.
    async fn run_alert_pass(
        &self,
        events: &[CalendarEvent],
        entity_id: &str,
        now: DateTime<Utc>,
    ) -> usize {
        use chrono::Timelike;

        if self.alert_sink.is_none() && !self.config.dry_run {
            return 0;
        }
        let cands: Vec<AlertCandidate> = events
            .iter()
            .filter_map(|ev| {
                AlertCandidate::from_event(ev, entity_id, &self.config.calendar_id)
            })
            .collect();

        let mut sent = 0usize;

        // Imminent starts (#396).
        let lead = chrono::Duration::minutes(self.config.alert_lead_min.max(0));
        for c in &cands {
            if !alerts::is_upcoming(c, now, lead) {
                continue;
            }
            let key = alerts::upcoming_key(&c.payload.event_id);
            let fp = alerts::upcoming_fingerprint(&c.payload);
            let text = alerts::render_upcoming(&c.payload, now);
            sent += self.deliver_alert(&key, &fp, &text).await;
        }

        // Overlaps (#397). Skip pairs whose overlap has already ended.
        for (x, y) in alerts::conflict_pairs(&cands) {
            let a = &cands[x].payload;
            let b = &cands[y].payload;
            if a.end.min(b.end) <= now {
                continue;
            }
            let key = alerts::conflict_key(a, b);
            let fp = alerts::conflict_fingerprint(a, b);
            let text = alerts::render_conflict(a, b);
            sent += self.deliver_alert(&key, &fp, &text).await;
        }

        // Once-per-day agenda (#396): first poll at/after the configured
        // local hour wins; the key is recorded even when the day is empty
        // so the check is deterministic.
        if let Some(hour) = self.config.agenda_local_hour {
            let local_now = now.with_timezone(&chrono::Local);
            if local_now.hour() >= hour {
                let key = alerts::agenda_key(local_now.date_naive());
                let already = self
                    .store
                    .calendar_alert_fingerprint(&key)
                    .unwrap_or_else(|e| {
                        warn!(key, "agenda dedup lookup failed: {e}");
                        Some("lookup-failed".into())
                    })
                    .is_some();
                if !already {
                    let items = alerts::agenda_items(&cands, now);
                    if items.is_empty() {
                        if !self.config.dry_run {
                            if let Err(e) =
                                self.store.upsert_calendar_alert(&key, "empty")
                            {
                                warn!(key, "agenda dedup record failed: {e}");
                            }
                        }
                    } else {
                        let text = alerts::render_agenda(&items, now);
                        sent += self.deliver_alert(&key, "sent", &text).await;
                    }
                }
            }
        }

        sent
    }

    /// Send one alert unless the store says this (key, fingerprint) pair
    /// already fired. The dedup row is written only after a successful
    /// send, so a transport failure retries on the next poll.
    async fn deliver_alert(&self, key: &str, fingerprint: &str, text: &str) -> usize {
        match self.store.calendar_alert_fingerprint(key) {
            Ok(Some(prev)) if prev == fingerprint => return 0,
            Ok(_) => {}
            Err(e) => {
                warn!(key, "calendar alert dedup lookup failed: {e}");
                return 0;
            }
        }
        if self.config.dry_run {
            println!("[gcal dry-run] would alert {key}: {text}");
            return 1;
        }
        let Some(sink) = &self.alert_sink else {
            return 0;
        };
        match sink.send(text).await {
            Ok(()) => {
                if let Err(e) = self.store.upsert_calendar_alert(key, fingerprint) {
                    warn!(key, "calendar alert sent but dedup record failed: {e}");
                }
                info!(key, "calendar alert sent");
                1
            }
            Err(e) => {
                warn!(key, "calendar alert send failed: {e:#}");
                0
            }
        }
    }

    /// One event → fan out to per-attendee wiki ingests, then mark the event
    /// id processed so subsequent polls dedup. Returns the number of ingest
    /// tasks spawned.
    async fn process_event(
        &self,
        payload: &MeetingPayload,
        my_email: &str,
    ) -> anyhow::Result<usize> {
        let attendees: Vec<_> = payload
            .attendees
            .iter()
            .filter(|a| !a.is_self)
            .filter(|a| !a.is_resource)
            .filter(|a| !a.email.eq_ignore_ascii_case(my_email))
            .collect();

        // Always upsert the per-event dedup row first. We use a stable
        // event-id key so polls inside the same hot window collapse to one
        // ingest pass even if attendee fan-out fails partway through.
        let dedup_email = synthetic_event_email(payload, my_email);
        if !self.config.dry_run {
            self.store.upsert_email(&dedup_email)?;
        } else {
            println!(
                "[gcal dry-run] would dedup event {} ({} attendees)",
                payload.event_id,
                attendees.len()
            );
        }

        let (Some(root), Some(schema)) = (&self.config.wiki_root, &self.wiki_schema)
        else {
            // No wiki: still mark processed so we don't re-tick this event.
            if !self.config.dry_run {
                self.store.mark_email_processed(
                    &payload.event_id,
                    TriageResult::DigestOnly,
                )?;
            }
            return Ok(0);
        };

        let mut spawned = 0usize;
        for a in attendees {
            let synthetic = synthetic_attendee_email(payload, a);
            if self.config.dry_run {
                println!(
                    "[gcal dry-run] would ingest event={} attendee={} subject={:?}",
                    payload.event_id, synthetic.from, synthetic.subject
                );
                continue;
            }
            // Per-attendee dedup row. Mirrors the per-event row's key
            // shape so a partial-fanout retry can complete missing
            // attendees without re-firing ingest for those already done.
            self.store.upsert_email(&synthetic)?;
            spawn_ingest(
                Arc::clone(&self.reasoner),
                root.clone(),
                schema.clone(),
                synthetic,
                DecisionKind::Meeting,
                None,
                None,
                IngestTrigger::Meeting,
            );
            spawned += 1;
        }

        if !self.config.dry_run {
            self.store
                .mark_email_processed(&payload.event_id, TriageResult::DigestOnly)?;
        }
        Ok(spawned)
    }
}

/// Synthetic dedup row written for the event itself (not per-attendee). The
/// `subject` line carries the truncated meeting title for any human
/// browsing the table; the `body` is empty by design — privacy-allowlisted
/// content lives only in attendee rows that the LLM actually reads.
fn synthetic_event_email(payload: &MeetingPayload, my_email: &str) -> Email {
    Email {
        message_id: payload.event_id.clone(),
        thread_id: payload.recurring_event_id.clone(),
        from: my_email.to_string(),
        subject: format!("Meeting: {}", truncate(&payload.summary, 80)),
        body: String::new(),
        date: payload.start.to_rfc3339(),
        account_entity_id: Some(payload.entity_id.clone()),
        platform: PLATFORM.into(),
        kind: "meeting".into(),
    }
}

/// Synthetic email shipped to wiki ingest, one per attendee. The body is
/// produced by [`render_meeting_body`] and carries only allowlisted fields
/// (attendee list + duration + conference kind) — never the event
/// description, never the raw location.
pub fn synthetic_attendee_email(
    payload: &MeetingPayload,
    attendee: &crate::types::MeetingAttendee,
) -> Email {
    let display_name = attendee
        .display_name
        .clone()
        .unwrap_or_else(|| attendee.email.clone());
    Email {
        message_id: format!("gcal:{}:{}", payload.event_id, attendee.email),
        thread_id: payload.recurring_event_id.clone(),
        from: format!("{} <{}>", display_name, attendee.email),
        subject: format!("Meeting: {}", truncate(&payload.summary, 80)),
        body: render_meeting_body(payload),
        date: payload.start.to_rfc3339(),
        account_entity_id: Some(payload.entity_id.clone()),
        platform: PLATFORM.into(),
        kind: "meeting".into(),
    }
}

/// Helper exposed for tests + external callers building one-shot WorkItems.
pub fn poll_window(now: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
    (
        now - chrono::Duration::hours(HOT_LOOKBACK_HOURS),
        now + chrono::Duration::hours(HOT_LOOKAHEAD_HOURS),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use augmentagent_channel_core::ReasonerOpts;
    use chrono::DateTime;

    use crate::gcal::CalendarError;
    use crate::types::{CalendarEvent, EventTime, RawAttendee, RawOrganizer};

    struct StubApi {
        events: Vec<CalendarEvent>,
    }

    #[async_trait]
    impl CalendarApi for StubApi {
        async fn list_events(
            &self,
            _e: &str,
            _c: &str,
            _t: DateTime<Utc>,
            _u: DateTime<Utc>,
        ) -> Result<Vec<CalendarEvent>, CalendarError> {
            Ok(self.events.clone())
        }
        async fn get_event(
            &self,
            _e: &str,
            _c: &str,
            _id: &str,
        ) -> Result<CalendarEvent, CalendarError> {
            Err(CalendarError::Decode("stub".into()))
        }
        async fn create_event(
            &self,
            _e: &str,
            _c: &str,
            _d: &crate::gcal::EventDraft,
        ) -> Result<crate::gcal::CreatedEvent, CalendarError> {
            Err(CalendarError::Decode("stub".into()))
        }
    }

    struct ForbiddenApi;
    #[async_trait]
    impl CalendarApi for ForbiddenApi {
        async fn list_events(
            &self,
            _e: &str,
            _c: &str,
            _t: DateTime<Utc>,
            _u: DateTime<Utc>,
        ) -> Result<Vec<CalendarEvent>, CalendarError> {
            Err(CalendarError::Forbidden {
                message: "insufficient_scope".into(),
            })
        }
        async fn get_event(
            &self,
            _e: &str,
            _c: &str,
            _id: &str,
        ) -> Result<CalendarEvent, CalendarError> {
            Err(CalendarError::Decode("stub".into()))
        }
        async fn create_event(
            &self,
            _e: &str,
            _c: &str,
            _d: &crate::gcal::EventDraft,
        ) -> Result<crate::gcal::CreatedEvent, CalendarError> {
            Err(CalendarError::Decode("stub".into()))
        }
    }

    struct StubReasoner;
    #[async_trait]
    impl Reasoner for StubReasoner {
        async fn call(&self, _opts: &ReasonerOpts, _u: &str) -> anyhow::Result<String> {
            Ok("ingested".into())
        }
    }

    fn tmp_store() -> (Arc<Store>, tempfile::NamedTempFile) {
        let file = tempfile::NamedTempFile::new().unwrap();
        {
            let conn = rusqlite::Connection::open(file.path()).unwrap();
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
                    firstSeenAt INTEGER NOT NULL, triageResult TEXT, agentProcessedAt INTEGER,
                    platform TEXT, kind TEXT
                );
                CREATE TABLE gmail_accounts (
                    id TEXT PRIMARY KEY, connectionId TEXT NOT NULL, email TEXT,
                    label TEXT, entityId TEXT NOT NULL, active INTEGER DEFAULT 1,
                    createdAt INTEGER NOT NULL
                );
                INSERT INTO gmail_accounts VALUES ('a1', 'c1', 'me@x.com', NULL, 'acc1', 1, 0);
                "#,
            )
            .unwrap();
        }
        (Arc::new(Store::open(file.path()).unwrap()), file)
    }

    fn standard_event(id: &str) -> CalendarEvent {
        CalendarEvent {
            id: id.into(),
            status: Some("confirmed".into()),
            summary: Some("Q3 planning".into()),
            description: Some("private NDA notes".into()),
            location: Some("221B Baker Street".into()),
            start: Some(EventTime {
                date_time: Some("2026-05-14T15:00:00Z".into()),
                ..Default::default()
            }),
            end: Some(EventTime {
                date_time: Some("2026-05-14T15:45:00Z".into()),
                ..Default::default()
            }),
            attendees: Some(vec![
                RawAttendee {
                    email: Some("me@x.com".into()),
                    self_: Some(true),
                    response_status: Some("accepted".into()),
                    ..Default::default()
                },
                RawAttendee {
                    email: Some("sarah@acme.com".into()),
                    display_name: Some("Sarah".into()),
                    response_status: Some("accepted".into()),
                    ..Default::default()
                },
                RawAttendee {
                    email: Some("ben@acme.com".into()),
                    display_name: Some("Ben".into()),
                    response_status: Some("tentative".into()),
                    ..Default::default()
                },
            ]),
            organizer: Some(RawOrganizer {
                email: Some("me@x.com".into()),
                self_: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn dry_run_records_no_dedup_no_ingest() {
        let (store, _f) = tmp_store();
        let api = Arc::new(StubApi {
            events: vec![standard_event("e1")],
        });
        let ch = CalendarChannel::new(
            store.clone(),
            api,
            Arc::new(StubReasoner),
            CalendarChannelConfig {
                dry_run: true,
                ..Default::default()
            },
        );
        let outcome = ch.poll_once().await.unwrap();
        assert_eq!(outcome.events_seen, 1);
        assert_eq!(outcome.new_events, 1);
        // Dedup row was NOT written in dry-run.
        assert!(!store.is_email_complete("e1").unwrap());
    }

    #[tokio::test]
    async fn live_run_writes_dedup_and_marks_processed() {
        let (store, _f) = tmp_store();
        let api = Arc::new(StubApi {
            events: vec![standard_event("e2")],
        });
        let ch = CalendarChannel::new(
            store.clone(),
            api,
            Arc::new(StubReasoner),
            CalendarChannelConfig {
                dry_run: false,
                ..Default::default()
            },
        );
        let outcome = ch.poll_once().await.unwrap();
        assert_eq!(outcome.new_events, 1);
        // Even with no wiki configured, mark_email_processed runs so the
        // next tick's `is_email_complete` gate trips.
        assert!(store.is_email_complete("e2").unwrap());

        // Second poll re-yields the same event, but the dedup gate skips it.
        let api2 = Arc::new(StubApi {
            events: vec![standard_event("e2")],
        });
        let ch2 = CalendarChannel::new(
            store.clone(),
            api2,
            Arc::new(StubReasoner),
            CalendarChannelConfig {
                dry_run: false,
                ..Default::default()
            },
        );
        let outcome2 = ch2.poll_once().await.unwrap();
        assert_eq!(outcome2.events_seen, 1);
        assert_eq!(outcome2.new_events, 0);
    }

    #[tokio::test]
    async fn forbidden_account_skipped_with_warning() {
        let (store, _f) = tmp_store();
        let api = Arc::new(ForbiddenApi);
        let ch = CalendarChannel::new(
            store,
            api,
            Arc::new(StubReasoner),
            CalendarChannelConfig {
                dry_run: false,
                ..Default::default()
            },
        );
        let outcome = ch.poll_once().await.unwrap();
        assert_eq!(outcome.forbidden_accounts, 1);
        assert_eq!(outcome.events_seen, 0);
        assert_eq!(outcome.errors, 0);
    }

    #[tokio::test]
    async fn synthetic_attendee_email_carries_no_description() {
        let payload = MeetingPayload::from_event(
            &standard_event("e1"),
            "ent1",
            "primary",
        )
        .unwrap();
        let attendee = payload
            .attendees
            .iter()
            .find(|a| a.email == "sarah@acme.com")
            .cloned()
            .unwrap();
        let email = synthetic_attendee_email(&payload, &attendee);
        for forbidden in ["NDA", "Baker Street", "221B", "private NDA"] {
            assert!(
                !email.body.contains(forbidden),
                "synthetic body leaked '{forbidden}': {}",
                email.body
            );
        }
        assert_eq!(email.platform, "gcal");
        assert_eq!(email.kind, "meeting");
        assert!(email.message_id.starts_with("gcal:e1:"));
    }

    // ---- #396/#397 alert pass ---------------------------------------------

    struct MockSink {
        sent: std::sync::Mutex<Vec<String>>,
    }

    impl MockSink {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                sent: std::sync::Mutex::new(Vec::new()),
            })
        }
        fn messages(&self) -> Vec<String> {
            self.sent.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl crate::alerts::AlertSink for MockSink {
        async fn send(&self, text: &str) -> anyhow::Result<()> {
            self.sent.lock().unwrap().push(text.to_string());
            Ok(())
        }
    }

    fn event_at(id: &str, start: DateTime<Utc>, end: DateTime<Utc>) -> CalendarEvent {
        let mut ev = standard_event(id);
        ev.start = Some(EventTime {
            date_time: Some(start.to_rfc3339()),
            ..Default::default()
        });
        ev.end = Some(EventTime {
            date_time: Some(end.to_rfc3339()),
            ..Default::default()
        });
        ev
    }

    fn alert_config() -> CalendarChannelConfig {
        CalendarChannelConfig {
            dry_run: false,
            alert_lead_min: 60,
            // Keep the (local-time-dependent) agenda out of these tests.
            agenda_local_hour: None,
            ..Default::default()
        }
    }

    fn channel_with(
        store: Arc<Store>,
        events: Vec<CalendarEvent>,
        sink: Arc<MockSink>,
        config: CalendarChannelConfig,
    ) -> CalendarChannel<StubApi, StubReasoner> {
        CalendarChannel::new(
            store,
            Arc::new(StubApi { events }),
            Arc::new(StubReasoner),
            config,
        )
        .with_alert_sink(sink)
    }

    #[tokio::test]
    async fn upcoming_alert_dedups_and_rearms_on_reschedule() {
        let (store, _f) = tmp_store();
        let sink = MockSink::new();
        let now = Utc::now();
        let start = now + chrono::Duration::minutes(30);
        let end = start + chrono::Duration::minutes(30);

        let ch = channel_with(
            store.clone(),
            vec![event_at("up1", start, end)],
            sink.clone(),
            alert_config(),
        );
        let o1 = ch.poll_once().await.unwrap();
        assert_eq!(o1.alerts_sent, 1, "first poll should alert");

        // Same event again: deduped.
        let ch2 = channel_with(
            store.clone(),
            vec![event_at("up1", start, end)],
            sink.clone(),
            alert_config(),
        );
        let o2 = ch2.poll_once().await.unwrap();
        assert_eq!(o2.alerts_sent, 0, "re-poll must not re-alert");

        // Rescheduled inside the lead window: re-armed.
        let start2 = now + chrono::Duration::minutes(45);
        let ch3 = channel_with(
            store.clone(),
            vec![event_at("up1", start2, start2 + chrono::Duration::minutes(30))],
            sink.clone(),
            alert_config(),
        );
        let o3 = ch3.poll_once().await.unwrap();
        assert_eq!(o3.alerts_sent, 1, "reschedule should re-alert");

        let msgs = sink.messages();
        assert_eq!(msgs.len(), 2);
        assert!(msgs[0].contains("starts in"), "{}", msgs[0]);
    }

    #[tokio::test]
    async fn conflict_alert_fires_once_per_pair() {
        let (store, _f) = tmp_store();
        let sink = MockSink::new();
        let now = Utc::now();
        // Both events beyond the 60-min lead so no upcoming alerts mix in.
        let a = event_at(
            "cfa",
            now + chrono::Duration::minutes(120),
            now + chrono::Duration::minutes(180),
        );
        let b = event_at(
            "cfb",
            now + chrono::Duration::minutes(150),
            now + chrono::Duration::minutes(210),
        );

        let ch = channel_with(
            store.clone(),
            vec![a.clone(), b.clone()],
            sink.clone(),
            alert_config(),
        );
        let o1 = ch.poll_once().await.unwrap();
        assert_eq!(o1.alerts_sent, 1, "overlap should alert once");
        assert!(sink.messages()[0].contains("Schedule conflict"));

        let ch2 = channel_with(store.clone(), vec![a, b], sink.clone(), alert_config());
        let o2 = ch2.poll_once().await.unwrap();
        assert_eq!(o2.alerts_sent, 0, "same overlap must not re-alert");
    }

    #[tokio::test]
    async fn dry_run_alert_pass_skips_sink_and_store() {
        let (store, _f) = tmp_store();
        let sink = MockSink::new();
        let now = Utc::now();
        let start = now + chrono::Duration::minutes(30);
        let mut config = alert_config();
        config.dry_run = true;

        let ch = channel_with(
            store.clone(),
            vec![event_at("dry1", start, start + chrono::Duration::minutes(30))],
            sink.clone(),
            config,
        );
        let outcome = ch.poll_once().await.unwrap();
        assert_eq!(outcome.alerts_sent, 1, "dry-run counts would-send alerts");
        assert!(sink.messages().is_empty(), "dry-run must not hit the sink");
        assert!(store
            .calendar_alert_fingerprint("upcoming:dry1")
            .unwrap()
            .is_none());
    }
}
