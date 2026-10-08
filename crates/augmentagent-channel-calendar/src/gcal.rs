//! Google Calendar client over Composio HTTP.
//!
//! Mirrors `crates/augmentagent-channel-email/src/gmail.rs::ComposioClient`:
//! one method per Composio action we use and the same `x-api-key` header.
//! Reads retry with bounded backoff; creates are never automatically replayed. The `CalendarApi` trait is the seam tests inject
//! a fake into.
//!
//! Phase 1 surfaces only `list_events` (the `GOOGLECALENDAR_EVENTS_LIST`
//! action) and `get_event` (`GOOGLECALENDAR_EVENTS_GET`). Recurrence-master
//! fetch + `CALENDARLIST_LIST` land in Phase 2 (#400).

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::warn;

use crate::types::CalendarEvent;

/// Proposed event for `GOOGLECALENDAR_CREATE_EVENT` (#398). This is the
/// machine payload the approval flow round-trips through sqlite: the
/// query-mode CLI serializes it into the `emails` row, and the daemon's
/// Approve handler parses it back and executes it. Keep it serde-stable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventDraft {
    pub summary: String,
    /// RFC3339 with offset, e.g. `2026-07-10T15:00:00-04:00`.
    pub start_datetime: String,
    pub duration_minutes: i64,
    /// Attendee emails; invites go out on create (`send_updates=true`).
    #[serde(default)]
    pub attendees: Vec<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// Attach a Google Meet room.
    #[serde(default)]
    pub create_meeting_room: bool,
}

/// Handle returned by a successful create.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CreatedEvent {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default, rename = "htmlLink")]
    pub html_link: Option<String>,
}

#[derive(Debug, Error)]
pub enum CalendarError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("composio: {message}")]
    Composio { message: String },
    /// Composio returned 403. The user likely needs to re-consent to grant
    /// `calendar.readonly` on top of the existing Google connection. Phase 1
    /// surfaces this once per account and skips the account; Phase 2 (#400)
    /// will wire the dashboard re-consent banner.
    #[error("forbidden: calendar scope likely missing — re-consent required ({message})")]
    Forbidden { message: String },
    #[error("decode: {0}")]
    Decode(String),
    #[error("invalid calendar input: {0}")]
    Invalid(String),
    /// A create might have reached Google. Never retry it automatically.
    #[error("calendar creation is unconfirmed: {0}; check the calendar before retrying")]
    Unconfirmed(String),
}

#[async_trait]
pub trait CalendarApi: Send + Sync {
    /// List events in `[time_min, time_max]` for the given calendar. Expands
    /// recurring series into instances (`singleEvents=true`) so each item
    /// carries `recurringEventId` when applicable. Pages until exhausted or
    /// `MAX_PAGES` is hit, whichever comes first.
    async fn list_events(
        &self,
        entity_id: &str,
        calendar_id: &str,
        time_min: DateTime<Utc>,
        time_max: DateTime<Utc>,
    ) -> Result<Vec<CalendarEvent>, CalendarError>;

    /// Fetch a single event by id. Used for recurrence-master lookup in
    /// Phase 2; kept on the Phase 1 trait so `ComposioCalendarClient` has
    /// only one impl block to maintain.
    async fn get_event(
        &self,
        entity_id: &str,
        calendar_id: &str,
        event_id: &str,
    ) -> Result<CalendarEvent, CalendarError>;

    /// #398 — create an event with attendees (`GOOGLECALENDAR_CREATE_EVENT`,
    /// `send_updates=true` so invites go out). Requires the `calendar.events`
    /// scope on the Google connection — a 403 surfaces as
    /// [`CalendarError::Forbidden`]. Only ever called from the daemon's
    /// Approve handler; there is no unattended write path.
    async fn create_event(
        &self,
        entity_id: &str,
        calendar_id: &str,
        draft: &EventDraft,
    ) -> Result<CreatedEvent, CalendarError>;
}

pub struct ComposioCalendarClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl ComposioCalendarClient {
    pub fn new(api_key: String) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("calendar HTTP client"),
            base_url: "https://backend.composio.dev".into(),
            api_key,
        }
    }

    pub fn with_base_url(mut self, base_url: String) -> Self {
        self.base_url = base_url;
        self
    }

    async fn execute(
        &self,
        action: &str,
        entity_id: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, CalendarError> {
        let url = format!("{}/api/v3/tools/execute/{}", self.base_url, action);
        let body = serde_json::json!({
            "user_id": entity_id,
            "arguments": arguments,
        });

        // Reads may retry; creates have no idempotency key. A lost response or
        // upstream 5xx must not create a second event/invitation (#1436).
        let creating = action == "GOOGLECALENDAR_CREATE_EVENT";
        let max_attempts = if creating { 1 } else { 3 };
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            let resp_result = self
                .http
                .post(&url)
                .header("x-api-key", &self.api_key)
                .json(&body)
                .send()
                .await;

            match resp_result {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        let value = resp.json::<serde_json::Value>().await.map_err(|e| {
                            if creating {
                                CalendarError::Unconfirmed(format!(
                                    "{action}: invalid or incomplete response ({e})"
                                ))
                            } else {
                                CalendarError::Http(e)
                            }
                        })?;
                        return check_envelope(action, value);
                    }
                    let text = bounded(&resp.text().await.unwrap_or_default(), 900);
                    if status.as_u16() == 403 {
                        return Err(CalendarError::Forbidden {
                            message: format!("{action} → 403: {text}"),
                        });
                    }
                    if creating && status.is_server_error() {
                        return Err(CalendarError::Unconfirmed(format!(
                            "{action} → {status}: {text}"
                        )));
                    }
                    let retryable = status.as_u16() == 429 || status.is_server_error();
                    let err = CalendarError::Composio {
                        message: format!("{action} → {status}: {text}"),
                    };
                    if retryable && attempt < max_attempts {
                        warn!(
                            action, status = %status, attempt,
                            "composio calendar retryable failure; backing off"
                        );
                        backoff(attempt).await;
                        continue;
                    }
                    return Err(err);
                }
                Err(e) if attempt < max_attempts && is_transient_reqwest(&e) => {
                    warn!(
                        action,
                        attempt, "composio calendar transport error; retrying: {e}"
                    );
                    backoff(attempt).await;
                    continue;
                }
                Err(e) if creating => {
                    return Err(CalendarError::Unconfirmed(format!("{action}: {e}")));
                }
                Err(e) => return Err(CalendarError::Http(e)),
            }
        }
    }
}

fn bounded(value: &str, max_chars: usize) -> String {
    let mut chars = value.trim().chars();
    let mut result: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        result.push('…');
    }
    result
}

/// Composio uses HTTP 200 for tool failures too. Reject those before any
/// result-shape fallback, including read paths that otherwise look empty.
fn check_envelope(action: &str, v: serde_json::Value) -> Result<serde_json::Value, CalendarError> {
    let error = [v.get("error"), v.pointer("/data/error")]
        .into_iter()
        .flatten()
        .find(|e| !e.is_null() && e.as_str().is_none_or(|s| !s.trim().is_empty()));
    let status = v
        .pointer("/data/status_code")
        .or_else(|| v.get("status_code"))
        .and_then(serde_json::Value::as_u64);
    if v.get("successful").and_then(serde_json::Value::as_bool) == Some(false)
        || error.is_some()
        || status.is_some_and(|s| s >= 400)
    {
        let detail = error.or_else(|| v.pointer("/data/message"));
        let detail = match detail {
            Some(serde_json::Value::String(s)) => bounded(s, 900),
            Some(value) => bounded(&value.to_string(), 900),
            None => "provider reported failure without an error message".into(),
        };
        let log_id = bounded(
            v.get("log_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("-"),
            128,
        );
        let message = format!("{action} reported failure: {detail} (composio log_id {log_id})");
        return Err(if status == Some(403) {
            CalendarError::Forbidden { message }
        } else {
            CalendarError::Composio { message }
        });
    }
    if v.get("successful").is_some_and(|flag| !flag.is_boolean()) {
        return Err(if action == "GOOGLECALENDAR_CREATE_EVENT" {
            CalendarError::Unconfirmed("provider returned an invalid successful flag".into())
        } else {
            CalendarError::Decode("provider returned an invalid successful flag".into())
        });
    }
    Ok(v)
}

fn is_transient_reqwest(e: &reqwest::Error) -> bool {
    e.is_timeout() || e.is_connect() || e.is_request()
}

async fn backoff(attempt: u32) {
    let base_ms: u64 = 300;
    let mult: u64 = 1 << attempt.min(5);
    let delay = std::time::Duration::from_millis(base_ms * mult);
    tokio::time::sleep(delay).await;
}

#[derive(Debug, Default, Deserialize)]
struct ListResp {
    #[serde(default)]
    data: ListData,
}

#[derive(Debug, Default, Deserialize)]
struct ListData {
    #[serde(default, alias = "events")]
    items: Vec<CalendarEvent>,
    #[serde(
        default,
        alias = "next_page_token",
        alias = "nextPageToken",
        alias = "page_token"
    )]
    next_page_token: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct GetResp {
    #[serde(default)]
    data: CalendarEvent,
}

#[async_trait]
impl CalendarApi for ComposioCalendarClient {
    async fn list_events(
        &self,
        entity_id: &str,
        calendar_id: &str,
        time_min: DateTime<Utc>,
        time_max: DateTime<Utc>,
    ) -> Result<Vec<CalendarEvent>, CalendarError> {
        const MAX_PAGES: u32 = 10;
        const PAGE_SIZE: u32 = 250;

        let mut collected: Vec<CalendarEvent> = Vec::new();
        let mut page_token: Option<String> = None;

        for _page in 0..MAX_PAGES {
            let mut args = serde_json::json!({
                "calendarId": calendar_id,
                "timeMin": time_min.to_rfc3339(),
                "timeMax": time_max.to_rfc3339(),
                "singleEvents": true,
                "orderBy": "startTime",
                "maxResults": PAGE_SIZE,
                "showDeleted": true,
            });
            if let Some(tok) = &page_token {
                args["pageToken"] = serde_json::Value::String(tok.clone());
            }

            let v = self
                .execute("GOOGLECALENDAR_EVENTS_LIST", entity_id, args)
                .await?;
            let parsed: ListResp = match serde_json::from_value(v.clone()) {
                Ok(r) => r,
                Err(_) => fallback_list_resp(&v),
            };

            let page_items = parsed.data.items;
            let token = parsed.data.next_page_token;
            if page_items.is_empty() && token.is_none() {
                break;
            }
            collected.extend(page_items);
            page_token = token;
            if page_token.is_none() {
                break;
            }
        }

        Ok(collected)
    }

    async fn get_event(
        &self,
        entity_id: &str,
        calendar_id: &str,
        event_id: &str,
    ) -> Result<CalendarEvent, CalendarError> {
        let args = serde_json::json!({
            "calendarId": calendar_id,
            "eventId": event_id,
        });
        let v = self
            .execute("GOOGLECALENDAR_EVENTS_GET", entity_id, args)
            .await?;
        if let Ok(GetResp { data }) = serde_json::from_value::<GetResp>(v.clone()) {
            return Ok(data);
        }
        if let Some(inner) = v.get("data").and_then(|d| d.get("response_data")).cloned() {
            if let Ok(ev) = serde_json::from_value::<CalendarEvent>(inner) {
                return Ok(ev);
            }
        }
        Err(CalendarError::Decode(format!(
            "events.get: unrecognised response shape: {}",
            serde_json::to_string(&v).unwrap_or_default()
        )))
    }

    async fn create_event(
        &self,
        entity_id: &str,
        calendar_id: &str,
        draft: &EventDraft,
    ) -> Result<CreatedEvent, CalendarError> {
        // Param names per Composio's GOOGLECALENDAR_CREATE_EVENT schema
        // (snake_case, unlike EVENTS_LIST's Google-native camelCase).
        // The Composio tool expects a naive datetime plus an IANA zone.
        // Passing an RFC3339 offset unchanged loses the offset upstream.
        let start = DateTime::parse_from_rfc3339(&draft.start_datetime)
            .map_err(|e| {
                CalendarError::Invalid(format!(
                    "start_datetime must be RFC3339 with an offset: {e}"
                ))
            })?
            .with_timezone(&Utc);
        let mins = draft.duration_minutes.max(1);
        let mut args = serde_json::json!({
            "calendar_id": calendar_id,
            "summary": draft.summary,
            "start_datetime": start.format("%Y-%m-%dT%H:%M:%S%.f").to_string(),
            "timezone": "UTC",
            "event_duration_hour": mins / 60,
            "event_duration_minutes": mins % 60,
            "attendees": draft.attendees,
            "send_updates": true,
        });
        if let Some(desc) = &draft.description {
            args["description"] = serde_json::Value::String(desc.clone());
        }
        args["create_meeting_room"] = serde_json::Value::Bool(draft.create_meeting_room);

        let v = self
            .execute("GOOGLECALENDAR_CREATE_EVENT", entity_id, args)
            .await?;
        // id + htmlLink live under data.response_data; tolerate flatter
        // shapes the same way the read paths do.
        let candidates = [
            v.get("data").and_then(|d| d.get("response_data")),
            v.get("data"),
            Some(&v),
        ];
        for cand in candidates.into_iter().flatten() {
            if let Ok(created) = serde_json::from_value::<CreatedEvent>(cand.clone()) {
                if created
                    .id
                    .as_deref()
                    .is_some_and(|id| !id.trim().is_empty())
                {
                    return Ok(created);
                }
            }
        }
        // No proof of creation. Do not manufacture success or retry a write
        // which may have happened; do not dump private event data into logs.
        let log_id = bounded(
            v.get("log_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("-"),
            128,
        );
        Err(CalendarError::Unconfirmed(format!(
            "GOOGLECALENDAR_CREATE_EVENT returned no nonempty event ID (composio log_id {log_id})"
        )))
    }
}

fn fallback_list_resp(v: &serde_json::Value) -> ListResp {
    let candidates: [&serde_json::Value; 3] = [
        v,
        v.get("data").unwrap_or(&serde_json::Value::Null),
        v.get("data")
            .and_then(|d| d.get("response_data"))
            .unwrap_or(&serde_json::Value::Null),
    ];
    for cand in candidates {
        if let Some(items_v) = cand.get("items").or_else(|| cand.get("events")) {
            if let Ok(items) = serde_json::from_value::<Vec<CalendarEvent>>(items_v.clone()) {
                let token = cand
                    .get("nextPageToken")
                    .or_else(|| cand.get("next_page_token"))
                    .and_then(|t| t.as_str())
                    .map(String::from);
                return ListResp {
                    data: ListData {
                        items,
                        next_page_token: token,
                    },
                };
            }
        }
    }
    ListResp::default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use mockito::Server;

    #[tokio::test]
    async fn list_events_parses_one_page() {
        let mut server = Server::new_async().await;
        let body = r#"{
          "data": {
            "items": [
              {
                "id": "evt-1",
                "iCalUID": "evt-1@example.com",
                "status": "confirmed",
                "summary": "Q3 planning",
                "start": { "dateTime": "2026-05-14T15:00:00Z" },
                "end":   { "dateTime": "2026-05-14T15:45:00Z" },
                "attendees": [
                  { "email": "me@x.example.com", "self": true, "responseStatus": "accepted" },
                  { "email": "sarah@acme.example.com", "displayName": "Sarah", "responseStatus": "accepted" }
                ],
                "organizer": { "email": "me@x.example.com", "self": true }
              }
            ]
          }
        }"#;
        let _m = server
            .mock("POST", "/api/v3/tools/execute/GOOGLECALENDAR_EVENTS_LIST")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(body)
            .create_async()
            .await;
        let client = ComposioCalendarClient::new("k".into()).with_base_url(server.url());
        let events = client
            .list_events(
                "ent",
                "primary",
                Utc.with_ymd_and_hms(2026, 5, 14, 0, 0, 0).unwrap(),
                Utc.with_ymd_and_hms(2026, 5, 15, 0, 0, 0).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, "evt-1");
        assert_eq!(events[0].summary.as_deref(), Some("Q3 planning"));
    }

    #[tokio::test]
    async fn list_events_paginates() {
        let mut server = Server::new_async().await;
        let body1 = r#"{
          "data": {
            "items": [{ "id": "e1", "status": "confirmed",
              "start": {"dateTime":"2026-05-14T10:00:00Z"},
              "end":   {"dateTime":"2026-05-14T11:00:00Z"} }],
            "nextPageToken": "tok2"
          }
        }"#;
        let body2 = r#"{
          "data": {
            "items": [{ "id": "e2", "status": "confirmed",
              "start": {"dateTime":"2026-05-14T12:00:00Z"},
              "end":   {"dateTime":"2026-05-14T13:00:00Z"} }]
          }
        }"#;
        let _m1 = server
            .mock("POST", "/api/v3/tools/execute/GOOGLECALENDAR_EVENTS_LIST")
            .with_status(200)
            .with_body(body1)
            .expect(1)
            .create_async()
            .await;
        let _m2 = server
            .mock("POST", "/api/v3/tools/execute/GOOGLECALENDAR_EVENTS_LIST")
            .with_status(200)
            .with_body(body2)
            .expect(1)
            .create_async()
            .await;
        let client = ComposioCalendarClient::new("k".into()).with_base_url(server.url());
        let events = client
            .list_events(
                "ent",
                "primary",
                Utc.with_ymd_and_hms(2026, 5, 14, 0, 0, 0).unwrap(),
                Utc.with_ymd_and_hms(2026, 5, 15, 0, 0, 0).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(events.len(), 2);
    }

    #[tokio::test]
    async fn create_event_sends_expected_args_and_parses_handle() {
        let mut server = Server::new_async().await;
        let _m = server
            .mock("POST", "/api/v3/tools/execute/GOOGLECALENDAR_CREATE_EVENT")
            .match_body(mockito::Matcher::AllOf(vec![
                mockito::Matcher::PartialJson(serde_json::json!({
                    "user_id": "ent",
                    "arguments": {
                        "calendar_id": "primary",
                        "summary": "Coffee chat",
                        "start_datetime": "2026-07-10T19:00:00",
                        "timezone": "UTC",
                        "event_duration_hour": 0,
                        "event_duration_minutes": 30,
                        "attendees": ["sarah@acme.example.com"],
                        "send_updates": true,
                        "create_meeting_room": false,
                    }
                })),
            ]))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"data":{"response_data":{"id":"new-evt-1","htmlLink":"https://calendar.google.com/event?eid=abc"}},"successful":true}"#,
            )
            .create_async()
            .await;
        let client = ComposioCalendarClient::new("k".into()).with_base_url(server.url());
        let draft = EventDraft {
            summary: "Coffee chat".into(),
            start_datetime: "2026-07-10T15:00:00-04:00".into(),
            duration_minutes: 30,
            attendees: vec!["sarah@acme.example.com".into()],
            description: None,
            create_meeting_room: false,
        };
        let created = client.create_event("ent", "primary", &draft).await.unwrap();
        assert_eq!(created.id.as_deref(), Some("new-evt-1"));
        assert!(created
            .html_link
            .as_deref()
            .unwrap()
            .contains("calendar.google.com"));
    }

    #[tokio::test]
    async fn create_event_forbidden_surfaces_scope_error() {
        let mut server = Server::new_async().await;
        let _m = server
            .mock("POST", "/api/v3/tools/execute/GOOGLECALENDAR_CREATE_EVENT")
            .with_status(403)
            .with_body("insufficient_scope: calendar.events")
            .create_async()
            .await;
        let client = ComposioCalendarClient::new("k".into()).with_base_url(server.url());
        let draft = EventDraft {
            summary: "x".into(),
            start_datetime: "2026-07-10T15:00:00-04:00".into(),
            duration_minutes: 90,
            attendees: vec![],
            description: None,
            create_meeting_room: false,
        };
        let err = client
            .create_event("ent", "primary", &draft)
            .await
            .unwrap_err();
        assert!(matches!(err, CalendarError::Forbidden { .. }));
    }

    #[tokio::test]
    async fn forbidden_surfaces_distinct_error() {
        let mut server = Server::new_async().await;
        let _m = server
            .mock("POST", "/api/v3/tools/execute/GOOGLECALENDAR_EVENTS_LIST")
            .with_status(403)
            .with_body("insufficient_scope")
            .create_async()
            .await;
        let client = ComposioCalendarClient::new("k".into()).with_base_url(server.url());
        let err = client
            .list_events(
                "ent",
                "primary",
                Utc.with_ymd_and_hms(2026, 5, 14, 0, 0, 0).unwrap(),
                Utc.with_ymd_and_hms(2026, 5, 15, 0, 0, 0).unwrap(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, CalendarError::Forbidden { .. }));
    }
}

#[cfg(test)]
#[path = "gcal_regression_tests.rs"]
mod regression_tests;
