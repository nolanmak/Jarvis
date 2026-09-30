//! Slack client over Composio's `/api/v3/tools/execute/{ACTION}` endpoint.
//!
//! Each method maps to one Composio SLACK_* tool. All calls carry the
//! `entity_id` from `SlackAuth` to route to the correct connected workspace.

use reqwest::Client;
use serde_json::{json, Value};
use thiserror::Error;
use tracing::debug;

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max { s.to_string() } else { format!("{}…", &s[..max]) }
}

use crate::auth::SlackAuth;
use crate::types::{Conversation, SlackMessage, SlackUser};

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct TeamInfo {
    pub team_id: String,
    pub team_name: String,
    pub team_domain: Option<String>,
}

const DEFAULT_BASE_URL: &str = "https://backend.composio.dev";

#[derive(Debug, Error)]
pub enum SlackError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("composio: {0}")]
    Composio(String),
    #[error("slack api: {0}")]
    Slack(String),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

pub struct SlackClient {
    auth: SlackAuth,
    http: Client,
    base_url: String,
}

/// **Tests and local QA only.** In a debug build, a loopback Composio base
/// URL (`http://127.0.0.1:<port>`) that every [`SlackClient::new`] uses
/// instead of the real Composio backend, so the Slack contact-reply send
/// can be shown end to end against a local fake. Ignored in release builds
/// and for any non-loopback value.
pub const TEST_COMPOSIO_BASE_ENV: &str = "AUGMENTAGENT_TEST_COMPOSIO_BASE";

/// The [`TEST_COMPOSIO_BASE_ENV`] override, when it may apply.
pub fn test_composio_base(value: Option<&str>) -> Option<String> {
    if !cfg!(debug_assertions) {
        return None;
    }
    let v = value.map(str::trim).filter(|v| !v.is_empty())?;
    let url = reqwest::Url::parse(v).ok()?;
    let loopback = match url.host_str()? {
        "localhost" => true,
        h => h
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback()),
    };
    (url.scheme() == "http" && loopback).then(|| v.trim_end_matches('/').to_string())
}

impl SlackClient {
    pub fn new(auth: SlackAuth) -> Result<Self, SlackError> {
        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        let base_url = match test_composio_base(
            std::env::var(TEST_COMPOSIO_BASE_ENV).ok().as_deref(),
        ) {
            Some(base) => {
                tracing::warn!("{TEST_COMPOSIO_BASE_ENV} is set: Composio Slack calls go to a local test server (debug build only)");
                base
            }
            None => DEFAULT_BASE_URL.into(),
        };
        Ok(Self {
            auth,
            http,
            base_url,
        })
    }

    /// Point the client at another Composio base URL (tests: a mock
    /// server).
    pub fn with_base_url(auth: SlackAuth, base_url: impl Into<String>) -> Self {
        Self {
            auth,
            http: Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .expect("reqwest"),
            base_url: base_url.into(),
        }
    }

    pub fn auth(&self) -> &SlackAuth {
        &self.auth
    }

    /// `SLACK_LIST_CONVERSATIONS` — DMs + channels user can see.
    /// `types` is a Slack-shaped CSV, e.g. `"public_channel,private_channel,im,mpim"`.
    pub async fn list_conversations(
        &self,
        types: &str,
        limit: u32,
    ) -> Result<Vec<Conversation>, SlackError> {
        let resp = self
            .execute(
                "SLACK_LIST_CONVERSATIONS",
                json!({ "types": types, "limit": limit, "exclude_archived": true }),
            )
            .await?;
        let channels = find_array(&resp, &["channels"])
            .ok_or_else(|| SlackError::Slack("no channels array in response".into()))?;
        let mut out = Vec::new();
        for raw in channels {
            if let Ok(c) = serde_json::from_value::<Conversation>(raw.clone()) {
                out.push(c);
            }
        }
        Ok(out)
    }

    /// `SLACK_FETCH_CONVERSATION_HISTORY` — messages in a channel.
    ///
    /// `oldest` is the Slack timestamp of the last-seen message; pass `None`
    /// on a fresh subscription to grab the most recent N messages.
    pub async fn fetch_messages(
        &self,
        channel_id: &str,
        oldest: Option<&str>,
        limit: u32,
    ) -> Result<Vec<SlackMessage>, SlackError> {
        let mut args = json!({
            "channel": channel_id,
            "limit": limit.clamp(1, 200),
        });
        if let Some(ts) = oldest {
            args["oldest"] = json!(ts);
        }
        let resp = self
            .execute("SLACK_FETCH_CONVERSATION_HISTORY", args)
            .await?;
        let messages = find_array(&resp, &["messages"])
            .ok_or_else(|| SlackError::Slack("no messages array in response".into()))?;
        let mut out = Vec::new();
        for raw in messages {
            if let Ok(m) = serde_json::from_value::<SlackMessage>(raw.clone()) {
                out.push(m);
            }
        }
        Ok(out)
    }

    /// `SLACK_FETCH_TEAM_INFO` — workspace metadata (team id, name, domain).
    /// Used at OAuth time to learn which workspace a freshly-connected
    /// account belongs to. Drills specifically into `data.team.{id,name,domain}`
    /// rather than a generic recursive search — Composio responses often
    /// carry multiple `id` fields (auth config id, connection id, etc.) and
    /// the first match would be wrong.
    pub async fn fetch_team_info(&self) -> Result<TeamInfo, SlackError> {
        let resp = self.execute("SLACK_FETCH_TEAM_INFO", json!({})).await?;
        // Try the most common Composio shapes in order:
        //   data.team.{id,name,domain}
        //   data.response_data.team.{...}
        //   response_data.team.{...}
        //   team.{...}
        let team = resp
            .pointer("/data/team")
            .or_else(|| resp.pointer("/data/response_data/team"))
            .or_else(|| resp.pointer("/response_data/team"))
            .or_else(|| resp.get("team"))
            .ok_or_else(|| {
                SlackError::Slack(format!(
                    "no team object in SLACK_FETCH_TEAM_INFO response: {}",
                    truncate(&resp.to_string(), 400)
                ))
            })?;
        let team_id = team
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                SlackError::Slack(format!(
                    "team object missing id: {}",
                    truncate(&team.to_string(), 400)
                ))
            })?
            .to_string();
        let team_name = team
            .get("name")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| team_id.clone());
        let team_domain = team
            .get("domain")
            .and_then(|v| v.as_str())
            .map(String::from);
        debug!(team_id = %team_id, team_name = %team_name, "fetch_team_info ok");
        Ok(TeamInfo {
            team_id,
            team_name,
            team_domain,
        })
    }

    /// `SLACK_USERS_LOOKUP_BY_EMAIL` would require an email; instead use
    /// `SLACK_RETRIEVE_CURRENT_USER_DETAILS` (Slack `auth.test`) which returns
    /// the authenticated user_id without arguments. Falls back gracefully if
    /// the action isn't available — user_id is only used for self-message
    /// filtering, so a missing value just means we don't dedup own messages.
    pub async fn fetch_authed_user_id(&self) -> Result<Option<String>, SlackError> {
        // Try the most common Composio action names in order.
        for action in [
            "SLACK_RETRIEVE_CURRENT_USER_DETAILS",
            "SLACK_AUTH_TEST",
            "SLACK_USERS_INFO_OF_THE_AUTHED_USER",
        ] {
            match self.execute(action, json!({})).await {
                Ok(resp) => {
                    if let Some(uid) = find_string(&resp, &["user_id"])
                        .or_else(|| find_string(&resp, &["id"]))
                    {
                        return Ok(Some(uid));
                    }
                }
                Err(SlackError::Composio(msg)) if msg.contains("404") || msg.contains("not found") => {
                    continue;
                }
                Err(_) => continue,
            }
        }
        Ok(None)
    }

    /// `SLACK_RETRIEVE_DETAILED_USER_INFORMATION` — resolve a user id to
    /// display name. Used for DM recipient labels.
    pub async fn get_user(&self, user_id: &str) -> Result<SlackUser, SlackError> {
        let resp = self
            .execute(
                "SLACK_RETRIEVE_DETAILED_USER_INFORMATION",
                json!({ "user": user_id }),
            )
            .await?;
        let user = find_value(&resp, &["user"])
            .ok_or_else(|| SlackError::Slack("no user in response".into()))?;
        serde_json::from_value::<SlackUser>(user.clone()).map_err(Into::into)
    }

    async fn execute(&self, action: &str, arguments: Value) -> Result<Value, SlackError> {
        let url = format!("{}/api/v3/tools/execute/{}", self.base_url, action);
        let body = json!({
            "user_id": self.auth.entity_id,
            "arguments": arguments,
        });
        let resp = self
            .http
            .post(&url)
            .header("x-api-key", &self.auth.composio_api_key)
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        debug!(action, %status, "composio slack call");
        if !status.is_success() {
            return Err(SlackError::Composio(format!(
                "{action} → {status}: {text}"
            )));
        }
        let json_val: Value = serde_json::from_str(&text)?;

        // Composio wraps the Slack response under `data.response_data`, but
        // shape varies across actions. Surface `successful: false` as an
        // error so callers don't silently proceed on Slack-side failures.
        if json_val
            .get("successful")
            .and_then(|v| v.as_bool())
            .is_some_and(|b| !b)
        {
            let err_msg = json_val
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("composio reported failure");
            return Err(SlackError::Composio(err_msg.to_string()));
        }

        Ok(json_val)
    }
}

/// #1290 — the Composio *user* connection is the owner's own Slack account:
/// contact messages go out through it with `as_user: true` ("For the Slack
/// toolkit, set `as_user=True` to post as the authenticated user",
/// docs.composio.dev/toolkits/slack, read 2026-09-29), never through the
/// interactive app's bot token.
#[async_trait::async_trait]
impl crate::contact::ContactSendApi for SlackClient {
    fn owner_user_id(&self) -> Option<String> {
        Some(self.auth.user_id.clone()).filter(|u| !u.trim().is_empty())
    }

    async fn post_as_owner(
        &self,
        message: &crate::contact::OutgoingContactMessage,
    ) -> Result<crate::contact::PostedContactMessage, crate::contact::ContactSendError> {
        use crate::contact::ContactSendError::{Rejected, Unknown};
        let mut args = json!({
            "channel": message.channel,
            "text": message.text,
            "as_user": true,
            // Model text never links user groups or `@channel`.
            "link_names": false,
        });
        if let Some(ts) = &message.thread_ts {
            args["thread_ts"] = json!(ts);
        }
        let url = format!("{}/api/v3/tools/execute/SLACK_SEND_MESSAGE", self.base_url);
        let resp = self
            .http
            .post(&url)
            .header("x-api-key", &self.auth.composio_api_key)
            .json(&json!({"user_id": self.auth.entity_id, "arguments": args}))
            .send()
            .await
            .map_err(|e| {
                // A connection that was never made carried nothing to Slack.
                if e.is_connect() && !e.is_timeout() {
                    Rejected(format!("could not reach Composio: {e}"))
                } else {
                    Unknown(format!("Composio request failed: {e}"))
                }
            })?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| Unknown(format!("Composio response unreadable: {e}")))?;
        if status.is_client_error() {
            return Err(Rejected(format!(
                "SLACK_SEND_MESSAGE → {status}: {}",
                truncate(&text, 300)
            )));
        }
        if !status.is_success() {
            return Err(Unknown(format!(
                "SLACK_SEND_MESSAGE → {status}: {}",
                truncate(&text, 300)
            )));
        }
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| Unknown(format!("Composio response is not JSON: {e}")))?;
        if v.get("successful").and_then(Value::as_bool) == Some(false) {
            let err = v
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("composio reported failure");
            return Err(Rejected(err.to_string()));
        }
        let ts = find_string(&v, &["ts"])
            .ok_or_else(|| Unknown("SLACK_SEND_MESSAGE returned no ts".into()))?;
        let channel = find_string(&v, &["channel"]).unwrap_or_else(|| message.channel.clone());
        let posted = find_value(&v, &["message"]);
        let field = |k: &str| {
            posted
                .and_then(|m| m.get(k))
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        Ok(crate::contact::PostedContactMessage {
            channel,
            ts,
            user: field("user"),
            bot_id: field("bot_id"),
        })
    }

    async fn find_owner_message(
        &self,
        channel: &str,
        thread_ts: Option<&str>,
        oldest_ts: &str,
        owner_user_id: &str,
        text: &str,
    ) -> Result<Option<String>, crate::contact::ContactSendError> {
        let (action, args) = match thread_ts {
            Some(ts) => (
                "SLACK_FETCH_MESSAGE_THREAD_FROM_A_CONVERSATION",
                json!({"channel": channel, "ts": ts, "oldest": oldest_ts, "limit": 200}),
            ),
            None => (
                "SLACK_FETCH_CONVERSATION_HISTORY",
                json!({"channel": channel, "oldest": oldest_ts, "limit": 200}),
            ),
        };
        let resp = self
            .execute(action, args)
            .await
            .map_err(|e| crate::contact::ContactSendError::Unknown(e.to_string()))?;
        let messages = find_array(&resp, &["messages"]).ok_or_else(|| {
            crate::contact::ContactSendError::Unknown(format!("{action} returned no messages"))
        })?;
        Ok(messages.iter().find_map(|m| {
            let by_owner = m.get("user").and_then(Value::as_str) == Some(owner_user_id);
            let same = m.get("text").and_then(Value::as_str) == Some(text);
            (by_owner && same)
                .then(|| m.get("ts").and_then(Value::as_str).map(str::to_string))
                .flatten()
        }))
    }
}

/// Find a nested array by walking through common Composio wrapper keys
/// (`data`, `response_data`) until we hit `field`.
fn find_array<'a>(value: &'a Value, fields: &[&str]) -> Option<&'a Vec<Value>> {
    find_by_keys(value, fields).and_then(|v| v.as_array())
}

fn find_string(value: &Value, fields: &[&str]) -> Option<String> {
    find_by_keys(value, fields)
        .and_then(|v| v.as_str())
        .map(String::from)
}

fn find_value<'a>(value: &'a Value, fields: &[&str]) -> Option<&'a Value> {
    find_by_keys(value, fields)
}

/// Recursive search for the first key in `fields` under `data` /
/// `response_data` / direct root.
fn find_by_keys<'a>(value: &'a Value, fields: &[&str]) -> Option<&'a Value> {
    fn walk<'v>(v: &'v Value, fields: &[&str], depth: u32) -> Option<&'v Value> {
        if depth > 6 {
            return None;
        }
        if let Value::Object(map) = v {
            for f in fields {
                if let Some(found) = map.get(*f) {
                    return Some(found);
                }
            }
            // Try common wrappers first, then everything else.
            for wrap in ["data", "response_data"] {
                if let Some(inner) = map.get(wrap) {
                    if let Some(found) = walk(inner, fields, depth + 1) {
                        return Some(found);
                    }
                }
            }
            for (_k, child) in map.iter() {
                if let Some(found) = walk(child, fields, depth + 1) {
                    return Some(found);
                }
            }
        }
        None
    }
    walk(value, fields, 0)
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_test_composio_base_is_debug_only_and_loopback_only() {
        use super::test_composio_base as base;
        if !cfg!(debug_assertions) {
            assert_eq!(base(Some("http://127.0.0.1:9")), None);
            return;
        }
        assert_eq!(
            base(Some("http://127.0.0.1:9/")).as_deref(),
            Some("http://127.0.0.1:9")
        );
        assert_eq!(
            base(Some("http://localhost:9")).as_deref(),
            Some("http://localhost:9")
        );
        assert_eq!(base(Some("http://example.com")), None);
        assert_eq!(base(Some("https://127.0.0.1:9")), None);
        assert_eq!(base(Some("")), None);
        assert_eq!(base(None), None);
    }

    use super::*;

    fn test_auth() -> SlackAuth {
        SlackAuth {
            entity_id: "eid".into(),
            connection_id: "cid".into(),
            team_id: "T1".into(),
            team_name: "Test".into(),
            user_id: "U1".into(),
            composio_api_key: "ckak_test".into(),
        }
    }

    #[tokio::test]
    async fn list_conversations_parses_nested_channels() {
        let mut server = mockito::Server::new_async().await;
        let body = json!({
            "successful": true,
            "data": {
                "response_data": {
                    "channels": [
                        { "id": "C1", "name": "general", "is_channel": true },
                        { "id": "D1", "is_im": true, "user": "U2" }
                    ]
                }
            }
        });
        let _m = server
            .mock("POST", "/api/v3/tools/execute/SLACK_LIST_CONVERSATIONS")
            .with_status(200)
            .with_body(body.to_string())
            .create_async()
            .await;

        let client = SlackClient::with_base_url(test_auth(), server.url());
        let convs = client
            .list_conversations("public_channel,im", 50)
            .await
            .unwrap();
        assert_eq!(convs.len(), 2);
        assert_eq!(convs[0].id, "C1");
        assert!(convs[1].is_im);
    }

    #[tokio::test]
    async fn fetch_messages_returns_user_messages() {
        let mut server = mockito::Server::new_async().await;
        let body = json!({
            "successful": true,
            "data": {
                "messages": [
                    { "type": "message", "user": "U2", "text": "hey", "ts": "1.000001" },
                    { "type": "message", "subtype": "channel_join", "user": "U2", "ts": "1.000002" }
                ]
            }
        });
        let _m = server
            .mock("POST", "/api/v3/tools/execute/SLACK_FETCH_CONVERSATION_HISTORY")
            .with_status(200)
            .with_body(body.to_string())
            .create_async()
            .await;

        let client = SlackClient::with_base_url(test_auth(), server.url());
        let msgs = client.fetch_messages("C1", None, 50).await.unwrap();
        assert_eq!(msgs.len(), 2);
        assert!(msgs[0].is_default_user_message());
        assert!(!msgs[1].is_default_user_message());
    }

    // ---------------------------------------------------------------
    // #1290 — contact sends through the Composio user connection
    // ---------------------------------------------------------------

    use crate::contact::{ContactSendApi, ContactSendError, OutgoingContactMessage};
    use mockito::Matcher;

    fn msg(thread: Option<&str>) -> OutgoingContactMessage {
        OutgoingContactMessage {
            channel: "C1".into(),
            thread_ts: thread.map(str::to_string),
            text: "*Yes*".into(),
        }
    }

    #[tokio::test]
    async fn a_contact_message_is_posted_as_the_user_in_its_thread() {
        let mut server = mockito::Server::new_async().await;
        let m = server
            .mock("POST", "/api/v3/tools/execute/SLACK_SEND_MESSAGE")
            .match_body(Matcher::PartialJson(json!({
                "user_id": "eid",
                "arguments": {"channel": "C1", "text": "*Yes*", "thread_ts": "1.000100",
                              "as_user": true, "link_names": false}
            })))
            .with_body(
                json!({"successful": true, "data": {"ok": true, "channel": "C1",
                    "ts": "2.000001", "message": {"user": "U1", "text": "*Yes*"}}})
                .to_string(),
            )
            .expect(1)
            .create_async()
            .await;
        let client = SlackClient::with_base_url(test_auth(), server.url());
        assert_eq!(client.owner_user_id().as_deref(), Some("U1"));
        let posted = client.post_as_owner(&msg(Some("1.000100"))).await.unwrap();
        m.assert_async().await;
        assert_eq!(posted.ts, "2.000001");
        assert_eq!(posted.channel, "C1");
        assert_eq!(posted.user.as_deref(), Some("U1"));
        assert_eq!(posted.bot_id, None);
    }

    #[tokio::test]
    async fn a_top_level_message_carries_no_thread_and_a_bot_attribution_is_reported() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("POST", "/api/v3/tools/execute/SLACK_SEND_MESSAGE")
            .match_request(|r| {
                let body: Value = serde_json::from_slice(r.body().unwrap()).unwrap();
                body["arguments"].get("thread_ts").is_none()
            })
            .with_body(
                json!({"successful": true, "data": {"response_data": {"ok": true, "channel": "C1",
                    "ts": "2.000002", "message": {"bot_id": "B1", "text": "x"}}}})
                .to_string(),
            )
            .create_async()
            .await;
        let client = SlackClient::with_base_url(test_auth(), server.url());
        let posted = client.post_as_owner(&msg(None)).await.unwrap();
        assert_eq!(posted.bot_id.as_deref(), Some("B1"));
        assert_eq!(posted.user, None);
    }

    #[tokio::test]
    async fn send_failures_say_whether_the_message_may_have_landed() {
        let mut server = mockito::Server::new_async().await;
        let client = SlackClient::with_base_url(test_auth(), server.url());
        let refused = server
            .mock("POST", "/api/v3/tools/execute/SLACK_SEND_MESSAGE")
            .with_body(json!({"successful": false, "error": "channel_not_found"}).to_string())
            .create_async()
            .await;
        assert!(matches!(
            client.post_as_owner(&msg(None)).await,
            Err(ContactSendError::Rejected(e)) if e.contains("channel_not_found")
        ));
        refused.remove_async().await;
        let bad_request = server
            .mock("POST", "/api/v3/tools/execute/SLACK_SEND_MESSAGE")
            .with_status(400)
            .with_body("bad")
            .create_async()
            .await;
        assert!(matches!(
            client.post_as_owner(&msg(None)).await,
            Err(ContactSendError::Rejected(_))
        ));
        bad_request.remove_async().await;
        let server_error = server
            .mock("POST", "/api/v3/tools/execute/SLACK_SEND_MESSAGE")
            .with_status(502)
            .with_body("gateway")
            .create_async()
            .await;
        assert!(matches!(
            client.post_as_owner(&msg(None)).await,
            Err(ContactSendError::Unknown(_))
        ));
        server_error.remove_async().await;
        let no_ts = server
            .mock("POST", "/api/v3/tools/execute/SLACK_SEND_MESSAGE")
            .with_body(json!({"successful": true, "data": {}}).to_string())
            .create_async()
            .await;
        assert!(matches!(
            client.post_as_owner(&msg(None)).await,
            Err(ContactSendError::Unknown(_))
        ));
        no_ts.remove_async().await;
        // Nothing listening: the request never left this host.
        let closed = SlackClient::with_base_url(test_auth(), "http://127.0.0.1:9");
        assert!(matches!(
            closed.post_as_owner(&msg(None)).await,
            Err(ContactSendError::Rejected(_))
        ));
    }

    #[tokio::test]
    async fn an_earlier_attempt_is_found_in_the_thread_or_the_channel() {
        let mut server = mockito::Server::new_async().await;
        let _thread = server
            .mock(
                "POST",
                "/api/v3/tools/execute/SLACK_FETCH_MESSAGE_THREAD_FROM_A_CONVERSATION",
            )
            .match_body(Matcher::PartialJson(json!({
                "arguments": {"channel": "C1", "ts": "1.000100", "oldest": "0.500000"}
            })))
            .with_body(
                json!({"successful": true, "data": {"messages": [
                    {"type": "message", "user": "U2", "text": "*Yes*", "ts": "1.000200"},
                    {"type": "message", "user": "U1", "text": "*Yes*", "ts": "1.000300"}
                ]}})
                .to_string(),
            )
            .create_async()
            .await;
        let _history = server
            .mock("POST", "/api/v3/tools/execute/SLACK_FETCH_CONVERSATION_HISTORY")
            .with_body(
                json!({"successful": true, "data": {"messages": [
                    {"type": "message", "user": "U1", "text": "other", "ts": "1.000400"}
                ]}})
                .to_string(),
            )
            .create_async()
            .await;
        let client = SlackClient::with_base_url(test_auth(), server.url());
        assert_eq!(
            client
                .find_owner_message("C1", Some("1.000100"), "0.500000", "U1", "*Yes*")
                .await
                .unwrap()
                .as_deref(),
            Some("1.000300")
        );
        assert_eq!(
            client
                .find_owner_message("C1", None, "0.500000", "U1", "*Yes*")
                .await
                .unwrap(),
            None
        );
    }
}
