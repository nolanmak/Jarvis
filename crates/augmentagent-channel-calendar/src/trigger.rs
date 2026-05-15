//! `CalendarTrigger`: implements the shared [`Trigger`] contract for the
//! Calendar event source.
//!
//! Phase 1 keeps the Trigger thin — `CalendarChannel::run` drives the poll
//! loop directly and per-attendee fan-out happens there. The trait
//! implementation exists primarily so the future `ChannelRunner` (when it
//! lands across 2+ channels) has a single shape to consume; right now
//! `next_work_items` materialises one `WorkItem` per Calendar event, with
//! the privacy-allowlisted [`MeetingPayload`] as its payload.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{Duration, Utc};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use augmentagent_channel_core::trigger::{Trigger, WorkItem};
use augmentagent_store::Store;

use crate::filter::passes_filter;
use crate::gcal::{CalendarApi, CalendarError};
use crate::types::MeetingPayload;
use crate::PLATFORM;

/// Hot-window: now-1h .. now+24h, per #82 §12.
pub const HOT_LOOKBACK_HOURS: i64 = 1;
pub const HOT_LOOKAHEAD_HOURS: i64 = 24;

pub struct CalendarTrigger<C: CalendarApi> {
    pub store: Arc<Store>,
    pub gcal: Arc<C>,
    pub calendar_id: String,
}

impl<C: CalendarApi> CalendarTrigger<C> {
    pub fn new(store: Arc<Store>, gcal: Arc<C>, calendar_id: String) -> Self {
        Self {
            store,
            gcal,
            calendar_id,
        }
    }
}

#[async_trait]
impl<C: CalendarApi + 'static> Trigger for CalendarTrigger<C> {
    async fn next_work_items(
        &self,
        _cancel: &CancellationToken,
    ) -> anyhow::Result<Vec<WorkItem>> {
        let accounts = self.store.get_active_gmail_accounts()?;
        if accounts.is_empty() {
            return Ok(Vec::new());
        }

        let now = Utc::now();
        let time_min = now - Duration::hours(HOT_LOOKBACK_HOURS);
        let time_max = now + Duration::hours(HOT_LOOKAHEAD_HOURS);

        let mut out: Vec<WorkItem> = Vec::new();
        for account in accounts {
            let events = match self
                .gcal
                .list_events(&account.entity_id, &self.calendar_id, time_min, time_max)
                .await
            {
                Ok(es) => es,
                Err(CalendarError::Forbidden { message }) => {
                    warn!(
                        account = %account.entity_id,
                        "calendar 403 — re-consent required for calendar.readonly: {message}"
                    );
                    continue;
                }
                Err(e) => {
                    warn!(account = %account.entity_id, "list_events failed: {e:#}");
                    continue;
                }
            };

            for ev in events {
                if let Err(reason) = passes_filter(&ev) {
                    debug!(
                        event_id = %ev.id,
                        skip = reason.label(),
                        "calendar event filtered out"
                    );
                    continue;
                }
                let Some(payload) =
                    MeetingPayload::from_event(&ev, &account.entity_id, &self.calendar_id)
                else {
                    debug!(event_id = %ev.id, "calendar event missing start/end; skipping");
                    continue;
                };
                let payload_json = match serde_json::to_value(&payload) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!(event_id = %ev.id, "serialize payload failed: {e}");
                        continue;
                    }
                };
                out.push(WorkItem {
                    platform: PLATFORM.into(),
                    kind: "meeting".into(),
                    external_id: payload.event_id.clone(),
                    payload: payload_json,
                });
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{CalendarEvent, EventTime, RawAttendee};
    use chrono::DateTime;

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

    fn one_event() -> CalendarEvent {
        CalendarEvent {
            id: "evt-1".into(),
            status: Some("confirmed".into()),
            summary: Some("Sync".into()),
            start: Some(EventTime {
                date_time: Some("2026-05-14T15:00:00Z".into()),
                ..Default::default()
            }),
            end: Some(EventTime {
                date_time: Some("2026-05-14T15:30:00Z".into()),
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
                    email: Some("a@y.com".into()),
                    response_status: Some("accepted".into()),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn yields_one_workitem_for_a_normal_event() {
        let (store, _f) = tmp_store();
        let api = Arc::new(StubApi {
            events: vec![one_event()],
        });
        let trigger = CalendarTrigger::new(store, api, "primary".into());
        let cancel = CancellationToken::new();
        let items = trigger.next_work_items(&cancel).await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].platform, "gcal");
        assert_eq!(items[0].kind, "meeting");
        assert_eq!(items[0].external_id, "evt-1");
    }

    #[tokio::test]
    async fn drops_filtered_events() {
        let (store, _f) = tmp_store();
        let mut bad = one_event();
        bad.id = "evt-cancelled".into();
        bad.status = Some("cancelled".into());
        let api = Arc::new(StubApi {
            events: vec![bad, one_event()],
        });
        let trigger = CalendarTrigger::new(store, api, "primary".into());
        let cancel = CancellationToken::new();
        let items = trigger.next_work_items(&cancel).await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].external_id, "evt-1");
    }
}
