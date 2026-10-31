//! Google Calendar over the operator's own Google OAuth client (#1584 port).
//!
//! A mailbox connected through the direct Google client (`google:<email>`
//! entity ids) already holds a grant that includes the `calendar` scope —
//! `augmentagent_channel_email::google::SCOPES` asks for it at consent so this
//! port needs no second consent. Calendar calls for those accounts go here,
//! straight to `www.googleapis.com/calendar/v3`, instead of through Composio,
//! whose googlecalendar connection is separate and can drop on its own.
//!
//! The wire shapes are Google's own (the Composio proxy path already spoke
//! them), so `CalendarEvent` / `CreatedEvent` parse unchanged.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use tracing::warn;

use crate::gcal::{CalendarError, CreatedEvent, EventDraft};
use crate::types::CalendarEvent;

const API_BASE: &str = "https://www.googleapis.com/calendar/v3";

/// Access tokens for a Google-connected mailbox. The seam tests stub.
#[async_trait]
pub trait GoogleTokens: Send + Sync {
    async fn access_token(&self, email: &str) -> Result<String, String>;
}

#[async_trait]
impl GoogleTokens for augmentagent_channel_email::google::GoogleGmail {
    async fn access_token(&self, email: &str) -> Result<String, String> {
        augmentagent_channel_email::google::GoogleGmail::access_token(self, email)
            .await
            .map_err(|e| e.to_string())
    }
}

pub struct GoogleCalendar {
    http: reqwest::Client,
    base: String,
    tokens: Arc<dyn GoogleTokens>,
}

impl GoogleCalendar {
    pub fn new(tokens: Arc<dyn GoogleTokens>) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("calendar HTTP client"),
            base: API_BASE.into(),
            tokens,
        }
    }

    pub fn with_base_url(mut self, base: String) -> Self {
        self.base = base;
        self
    }

    /// One Calendar API call as `email`. Reads retry 429/5xx/transport
    /// errors; writes are sent once, and a create whose outcome is unknown
    /// (transport error, 5xx, unreadable body) is `Unconfirmed` (#1436).
    pub async fn call(
        &self,
        email: &str,
        method: &str,
        path: &str,
        query: &[(&str, &str)],
        body: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, CalendarError> {
        let token = self.tokens.access_token(email).await.map_err(|e| CalendarError::Google {
            message: format!("Google credential for {email}: {e}"),
        })?;
        let read = method == "GET";
        let creating = method == "POST";
        let max_attempts = if read { 3 } else { 1 };
        let method_v = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|e| CalendarError::Invalid(format!("method {method}: {e}")))?;
        let url = format!("{}/{}", self.base, path);
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            let mut req = self
                .http
                .request(method_v.clone(), &url)
                .bearer_auth(&token)
                .query(query);
            if let Some(b) = &body {
                req = req.json(b);
            }
            let resp = match req.send().await {
                Ok(r) => r,
                Err(e) if attempt < max_attempts => {
                    warn!(method, path, attempt, "google calendar transport error; retrying: {e}");
                    tokio::time::sleep(std::time::Duration::from_millis(250 * u64::from(attempt))).await;
                    continue;
                }
                Err(e) if creating => {
                    return Err(CalendarError::Unconfirmed(format!("calendar {method} {path}: {e}")))
                }
                Err(e) => return Err(CalendarError::Http(e)),
            };
            let status = resp.status();
            if status.is_success() {
                return resp.json::<serde_json::Value>().await.map_err(|e| {
                    if creating {
                        CalendarError::Unconfirmed(format!("calendar {method} {path}: unreadable response ({e})"))
                    } else {
                        CalendarError::Http(e)
                    }
                });
            }
            let text = resp.text().await.unwrap_or_default();
            let detail = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|v| v.pointer("/error/message").and_then(|m| m.as_str()).map(str::to_string))
                .unwrap_or_else(|| text.chars().take(300).collect());
            let message = format!("calendar {method} {path} → {status}: {detail}");
            // 401 with a freshly minted token means the grant itself lacks
            // access (scope or revoked consent): same remedy as 403.
            if status.as_u16() == 403 || status.as_u16() == 401 {
                return Err(CalendarError::Forbidden { message });
            }
            if creating && status.is_server_error() {
                return Err(CalendarError::Unconfirmed(message));
            }
            if (status.as_u16() == 429 || status.is_server_error()) && attempt < max_attempts {
                warn!(method, path, %status, attempt, "google calendar retryable failure; backing off");
                tokio::time::sleep(std::time::Duration::from_millis(500 * u64::from(attempt))).await;
                continue;
            }
            return Err(CalendarError::Google { message });
        }
    }

    /// A call whose success has no body (`events.delete` → 204).
    pub async fn call_no_content(&self, email: &str, method: &str, path: &str) -> Result<(), CalendarError> {
        let token = self.tokens.access_token(email).await.map_err(|e| CalendarError::Google {
            message: format!("Google credential for {email}: {e}"),
        })?;
        let method_v = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|e| CalendarError::Invalid(format!("method {method}: {e}")))?;
        let resp = self.http.request(method_v, format!("{}/{}", self.base, path)).bearer_auth(&token).send().await?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        let message = format!("calendar {method} {path} → {status}");
        Err(if status.as_u16() == 403 || status.as_u16() == 401 {
            CalendarError::Forbidden { message }
        } else {
            CalendarError::Google { message }
        })
    }

    pub async fn list_events(
        &self,
        email: &str,
        calendar_id: &str,
        time_min: DateTime<Utc>,
        time_max: DateTime<Utc>,
    ) -> Result<Vec<CalendarEvent>, CalendarError> {
        const MAX_PAGES: u32 = 10;
        let path = format!("calendars/{}/events", crate::gcal::path_segment(calendar_id));
        let (min, max) = (time_min.to_rfc3339(), time_max.to_rfc3339());
        let mut collected = Vec::new();
        let mut page_token: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut query = vec![
                ("timeMin", min.as_str()),
                ("timeMax", max.as_str()),
                ("singleEvents", "true"),
                ("orderBy", "startTime"),
                ("maxResults", "250"),
                ("showDeleted", "true"),
            ];
            if let Some(t) = page_token.as_deref() {
                query.push(("pageToken", t));
            }
            let v = self.call(email, "GET", &path, &query, None).await?;
            let items: Vec<CalendarEvent> = match v.get("items") {
                Some(items) => serde_json::from_value(items.clone())
                    .map_err(|e| CalendarError::Decode(format!("events.list items: {e}")))?,
                None => Vec::new(),
            };
            collected.extend(items);
            page_token = v.get("nextPageToken").and_then(|t| t.as_str()).map(str::to_string);
            if page_token.is_none() {
                break;
            }
        }
        Ok(collected)
    }

    pub async fn create_event(
        &self,
        email: &str,
        calendar_id: &str,
        draft: &EventDraft,
    ) -> Result<CreatedEvent, CalendarError> {
        let start = DateTime::parse_from_rfc3339(&draft.start_datetime).map_err(|e| {
            CalendarError::Invalid(format!("start_datetime must be RFC3339 with an offset: {e}"))
        })?;
        let end = start + Duration::minutes(draft.duration_minutes.max(1));
        let mut body = serde_json::json!({
            "summary": draft.summary,
            "start": {"dateTime": start.to_rfc3339()},
            "end": {"dateTime": end.to_rfc3339()},
            "attendees": draft.attendees.iter().map(|a| serde_json::json!({"email": a})).collect::<Vec<_>>(),
        });
        if let Some(desc) = &draft.description {
            body["description"] = serde_json::Value::String(desc.clone());
        }
        if draft.create_meeting_room {
            body["conferenceData"] = serde_json::json!({"createRequest": {
                "requestId": uuid::Uuid::new_v4().to_string(),
                "conferenceSolutionKey": {"type": "hangoutsMeet"},
            }});
        }
        let path = format!("calendars/{}/events", crate::gcal::path_segment(calendar_id));
        // sendUpdates=all: invitations go out, as the Composio tool's
        // send_updates=true did. conferenceDataVersion=1 lets the Meet
        // request through (Google ignores conferenceData without it).
        let v = self
            .call(email, "POST", &path, &[("sendUpdates", "all"), ("conferenceDataVersion", "1")], Some(body))
            .await?;
        serde_json::from_value::<CreatedEvent>(v)
            .ok()
            .filter(|c| c.id.as_deref().is_some_and(|id| !id.trim().is_empty()))
            .ok_or_else(|| CalendarError::Unconfirmed("events.insert returned no event id".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Matcher, Server};

    struct Fixed;
    #[async_trait]
    impl GoogleTokens for Fixed {
        async fn access_token(&self, email: &str) -> Result<String, String> {
            assert_eq!(email, "owner@example.com");
            Ok("at-1".into())
        }
    }
    struct Revoked;
    #[async_trait]
    impl GoogleTokens for Revoked {
        async fn access_token(&self, _: &str) -> Result<String, String> {
            Err("grant revoked; reconnect from the dashboard".into())
        }
    }

    fn client(server: &Server) -> GoogleCalendar {
        GoogleCalendar::new(Arc::new(Fixed)).with_base_url(server.url())
    }

    #[tokio::test]
    async fn list_pages_with_the_bearer_token_and_google_native_params() {
        let mut server = Server::new_async().await;
        let first = server
            .mock("GET", "/calendars/primary/events")
            .match_header("authorization", "Bearer at-1")
            .match_query(Matcher::AllOf(vec![
                Matcher::UrlEncoded("singleEvents".into(), "true".into()),
                Matcher::UrlEncoded("orderBy".into(), "startTime".into()),
                Matcher::UrlEncoded("showDeleted".into(), "true".into()),
            ]))
            .with_body(r#"{"items":[{"id":"e1","status":"confirmed","start":{"dateTime":"2026-10-30T13:00:00-04:00"},"end":{"dateTime":"2026-10-30T13:30:00-04:00"}}],"nextPageToken":"p2"}"#)
            // mockito hands a request to the first matching mock still owed
            // hits, so page 2 (which also matches this mock) goes to `second`.
            .expect(1)
            .create_async()
            .await;
        let second = server
            .mock("GET", "/calendars/primary/events")
            .match_query(Matcher::UrlEncoded("pageToken".into(), "p2".into()))
            .with_body(r#"{"items":[{"id":"e2","status":"confirmed","start":{"date":"2026-10-31"},"end":{"date":"2026-11-01"}}]}"#)
            .expect(1)
            .create_async()
            .await;
        let now = Utc::now();
        let events = client(&server)
            .list_events("owner@example.com", "primary", now, now + Duration::days(1))
            .await
            .unwrap();
        assert_eq!(events.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["e1", "e2"]);
        first.assert_async().await;
        second.assert_async().await;
    }

    #[tokio::test]
    async fn create_sends_invites_once_with_an_end_time_and_a_meet_request() {
        let mut server = Server::new_async().await;
        let insert = server
            .mock("POST", "/calendars/primary/events")
            .match_query(Matcher::AllOf(vec![
                Matcher::UrlEncoded("sendUpdates".into(), "all".into()),
                Matcher::UrlEncoded("conferenceDataVersion".into(), "1".into()),
            ]))
            .match_body(Matcher::PartialJson(serde_json::json!({
                "summary": "Intro",
                "start": {"dateTime": "2026-10-30T10:00:00-07:00"},
                "end": {"dateTime": "2026-10-30T10:30:00-07:00"},
                "attendees": [{"email": "a@example.com"}, {"email": "b@example.com"}],
                "conferenceData": {"createRequest": {"conferenceSolutionKey": {"type": "hangoutsMeet"}}},
            })))
            .with_body(r#"{"id":"new1","htmlLink":"https://calendar.google.com/event?eid=x","attendees":[{"email":"owner@example.com","self":true,"responseStatus":"accepted"}]}"#)
            .expect(1)
            .create_async()
            .await;
        let draft = EventDraft {
            summary: "Intro".into(),
            start_datetime: "2026-10-30T10:00:00-07:00".into(),
            duration_minutes: 30,
            attendees: vec!["a@example.com".into(), "b@example.com".into()],
            description: None,
            create_meeting_room: true,
        };
        let created = client(&server).create_event("owner@example.com", "primary", &draft).await.unwrap();
        assert_eq!(created.id.as_deref(), Some("new1"));
        assert!(created.attendees.is_some());
        insert.assert_async().await;
    }

    #[tokio::test]
    async fn a_create_that_fails_upstream_is_unconfirmed_and_never_retried() {
        let mut server = Server::new_async().await;
        let insert = server
            .mock("POST", "/calendars/primary/events")
            .match_query(Matcher::Any)
            .with_status(503)
            .expect(1)
            .create_async()
            .await;
        let draft = EventDraft {
            summary: "x".into(), start_datetime: "2026-10-30T10:00:00-07:00".into(), duration_minutes: 30,
            attendees: vec![], description: None, create_meeting_room: false,
        };
        let err = client(&server).create_event("owner@example.com", "primary", &draft).await.unwrap_err();
        assert!(matches!(err, CalendarError::Unconfirmed(_)), "{err}");
        insert.assert_async().await;
    }

    #[tokio::test]
    async fn missing_scope_is_forbidden_and_reads_retry_transient_failures() {
        let mut server = Server::new_async().await;
        let _denied = server
            .mock("GET", "/calendars/primary/events/e1")
            .with_status(403)
            .with_body(r#"{"error":{"message":"Request had insufficient authentication scopes."}}"#)
            .create_async()
            .await;
        let err = client(&server).call("owner@example.com", "GET", "calendars/primary/events/e1", &[], None).await.unwrap_err();
        assert!(matches!(&err, CalendarError::Forbidden { message } if message.contains("insufficient")), "{err}");

        let flaky = server.mock("GET", "/calendars/primary/events/e2").with_status(503).expect(3).create_async().await;
        let err = client(&server).call("owner@example.com", "GET", "calendars/primary/events/e2", &[], None).await.unwrap_err();
        assert!(matches!(err, CalendarError::Google { .. }), "{err}");
        flaky.assert_async().await;
    }

    #[tokio::test]
    async fn google_connected_accounts_bypass_composio_and_others_still_use_it() {
        let mut google = Server::new_async().await;
        let mut composio = Server::new_async().await;
        let direct = google
            .mock("GET", "/calendars/primary/events")
            .match_query(Matcher::Any)
            .with_body(r#"{"items":[]}"#)
            .expect(1)
            .create_async()
            .await;
        let tool = composio
            .mock("POST", "/api/v3/tools/execute/GOOGLECALENDAR_EVENTS_LIST")
            .with_body(r#"{"successful":true,"data":{"items":[]}}"#)
            .expect(1)
            .create_async()
            .await;
        let client = crate::gcal::ComposioCalendarClient::new("k".into())
            .with_base_url(composio.url())
            .with_google(GoogleCalendar::new(Arc::new(Fixed)).with_base_url(google.url()));
        use crate::gcal::CalendarApi;
        let now = Utc::now();
        client.list_events("google:owner@example.com", "primary", now, now + Duration::hours(1)).await.unwrap();
        client.list_events("composio-entity-1", "primary", now, now + Duration::hours(1)).await.unwrap();
        direct.assert_async().await;
        tool.assert_async().await;
    }

    #[tokio::test]
    async fn a_revoked_grant_names_the_mailbox_without_calling_google() {
        let server = Server::new_async().await;
        let c = GoogleCalendar::new(Arc::new(Revoked)).with_base_url(server.url());
        let err = c.call("owner@example.com", "GET", "calendars/primary/events", &[], None).await.unwrap_err();
        assert!(err.to_string().contains("owner@example.com") && err.to_string().contains("revoked"), "{err}");
    }

    /// Opt-in live check against a real Google-connected mailbox: create a
    /// five-minute event with no attendees (so no invitation is sent), read
    /// it back, delete it. Needs GOOGLE_OAUTH_CLIENT_ID/SECRET in the env and
    /// the mailbox's stored grant.
    #[tokio::test]
    #[ignore = "live Google Calendar write on the owner's mailbox; set JARVIS_GCAL_DIRECT_QA_ACCOUNT"]
    async fn live_create_read_delete_on_a_google_connected_mailbox() {
        let email = std::env::var("JARVIS_GCAL_DIRECT_QA_ACCOUNT").expect("QA account");
        let google = augmentagent_channel_email::google::GoogleGmail::from_env().expect("OAuth client env");
        let cal = GoogleCalendar::new(Arc::new(google));
        let start = (Utc::now() + Duration::days(2)).with_timezone(&chrono::FixedOffset::west_opt(7 * 3600).unwrap());
        let start = start.format("%Y-%m-%dT%H:%M:00%:z").to_string();
        let marker = format!("[TEST direct-gcal] {}", &uuid::Uuid::new_v4().simple().to_string()[..12]);
        let draft = EventDraft {
            summary: marker.clone(), start_datetime: start, duration_minutes: 5,
            attendees: vec![], description: Some("automated QA; deleted immediately".into()), create_meeting_room: true,
        };
        let created = cal.create_event(&email, "primary", &draft).await.expect("create");
        let id = created.id.clone().unwrap();
        let path = format!("calendars/primary/events/{id}");
        let read = cal.call(&email, "GET", &path, &[], None).await.expect("read back");
        let deleted = cal.call_no_content(&email, "DELETE", &path).await;
        assert_eq!(read["summary"], marker.as_str());
        assert!(read.pointer("/conferenceData/entryPoints/0/uri").and_then(|u| u.as_str()).is_some_and(|u| u.starts_with("https://meet.google.com/")),
            "Meet link attached: {}", read["conferenceData"]);
        deleted.expect("delete");
        println!("created+deleted {id} ({marker}) on {email}");
    }
}
