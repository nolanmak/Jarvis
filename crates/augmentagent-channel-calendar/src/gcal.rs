//! Google Calendar client over Composio HTTP.
//!
//! Mirrors `crates/augmentagent-channel-email/src/gmail.rs::ComposioClient`:
//! one method per Composio action we use, identical retry+backoff loop, the
//! same `x-api-key` header. The `CalendarApi` trait is the seam tests inject
//! a fake into.
//!
//! Phase 1 surfaces only `list_events` (the `GOOGLECALENDAR_EVENTS_LIST`
//! action) and `get_event` (`GOOGLECALENDAR_EVENTS_GET`). Recurrence-master
//! fetch + `CALENDARLIST_LIST` land in Phase 2.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use thiserror::Error;
use tracing::warn;

use crate::types::CalendarEvent;

#[derive(Debug, Error)]
pub enum CalendarError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("composio: {message}")]
    Composio { message: String },
    /// Composio returned 403. The user likely needs to re-consent to grant
    /// `calendar.readonly` on top of the existing Google connection. Phase 1
    /// surfaces this once per account and skips the account; Phase 2 will
    /// wire the dashboard re-consent banner.
    #[error("forbidden: calendar scope likely missing — re-consent required ({message})")]
    Forbidden { message: String },
    #[error("decode: {0}")]
    Decode(String),
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
}

pub struct ComposioCalendarClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl ComposioCalendarClient {
    pub fn new(api_key: String) -> Self {
        Self {
            http: reqwest::Client::new(),
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

        const MAX_ATTEMPTS: u32 = 3;
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
                        return resp.json::<serde_json::Value>().await.map_err(Into::into);
                    }
                    let text = resp.text().await.unwrap_or_default();
                    if status.as_u16() == 403 {
                        return Err(CalendarError::Forbidden {
                            message: format!("{action} → 403: {text}"),
                        });
                    }
                    let retryable = status.as_u16() == 429 || status.is_server_error();
                    let err = CalendarError::Composio {
                        message: format!("{action} → {status}: {text}"),
                    };
                    if retryable && attempt < MAX_ATTEMPTS {
                        warn!(
                            action, status = %status, attempt,
                            "composio calendar retryable failure; backing off"
                        );
                        backoff(attempt).await;
                        continue;
                    }
                    return Err(err);
                }
                Err(e) if attempt < MAX_ATTEMPTS && is_transient_reqwest(&e) => {
                    warn!(
                        action, attempt,
                        "composio calendar transport error; retrying: {e}"
                    );
                    backoff(attempt).await;
                    continue;
                }
                Err(e) => return Err(CalendarError::Http(e)),
            }
        }
    }
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
                "showDeleted": false,
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
        if let Some(inner) = v
            .get("data")
            .and_then(|d| d.get("response_data"))
            .cloned()
        {
            if let Ok(ev) = serde_json::from_value::<CalendarEvent>(inner) {
                return Ok(ev);
            }
        }
        Err(CalendarError::Decode(format!(
            "events.get: unrecognised response shape: {}",
            serde_json::to_string(&v).unwrap_or_default()
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
            if let Ok(items) =
                serde_json::from_value::<Vec<CalendarEvent>>(items_v.clone())
            {
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
                "iCalUID": "evt-1@google.com",
                "status": "confirmed",
                "summary": "Q3 planning",
                "start": { "dateTime": "2026-05-14T15:00:00Z" },
                "end":   { "dateTime": "2026-05-14T15:45:00Z" },
                "attendees": [
                  { "email": "me@x.com", "self": true, "responseStatus": "accepted" },
                  { "email": "sarah@acme.com", "displayName": "Sarah", "responseStatus": "accepted" }
                ],
                "organizer": { "email": "me@x.com", "self": true }
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
        let client =
            ComposioCalendarClient::new("k".into()).with_base_url(server.url());
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
        let client =
            ComposioCalendarClient::new("k".into()).with_base_url(server.url());
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
    async fn forbidden_surfaces_distinct_error() {
        let mut server = Server::new_async().await;
        let _m = server
            .mock("POST", "/api/v3/tools/execute/GOOGLECALENDAR_EVENTS_LIST")
            .with_status(403)
            .with_body("insufficient_scope")
            .create_async()
            .await;
        let client =
            ComposioCalendarClient::new("k".into()).with_base_url(server.url());
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
