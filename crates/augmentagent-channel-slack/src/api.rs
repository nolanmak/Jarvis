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

impl SlackClient {
    pub fn new(auth: SlackAuth) -> Result<Self, SlackError> {
        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        Ok(Self {
            auth,
            http,
            base_url: DEFAULT_BASE_URL.into(),
        })
    }

    /// Testing-only constructor — override the Composio base URL for mockito.
    #[cfg(test)]
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

    /// `SLACK_SEND_MESSAGE` — post text to a channel/DM.
    pub async fn send_message(
        &self,
        channel_id: &str,
        text: &str,
    ) -> Result<String, SlackError> {
        let resp = self
            .execute(
                "SLACK_SEND_MESSAGE",
                json!({ "channel": channel_id, "text": text }),
            )
            .await?;
        // Slack's chat.postMessage returns { ok, ts, channel, message }.
        find_string(&resp, &["ts"])
            .ok_or_else(|| SlackError::Slack("send_message returned no ts".into()))
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

    #[tokio::test]
    async fn send_message_returns_ts() {
        let mut server = mockito::Server::new_async().await;
        let body = json!({
            "successful": true,
            "data": {
                "response_data": {
                    "ok": true,
                    "ts": "1234567890.000001",
                    "channel": "C1"
                }
            }
        });
        let _m = server
            .mock("POST", "/api/v3/tools/execute/SLACK_SEND_MESSAGE")
            .with_status(200)
            .with_body(body.to_string())
            .create_async()
            .await;

        let client = SlackClient::with_base_url(test_auth(), server.url());
        let ts = client.send_message("C1", "hello").await.unwrap();
        assert_eq!(ts, "1234567890.000001");
    }

    #[tokio::test]
    async fn unsuccessful_response_surfaces_error() {
        let mut server = mockito::Server::new_async().await;
        let body = json!({
            "successful": false,
            "error": "not_in_channel"
        });
        let _m = server
            .mock("POST", "/api/v3/tools/execute/SLACK_SEND_MESSAGE")
            .with_status(200)
            .with_body(body.to_string())
            .create_async()
            .await;

        let client = SlackClient::with_base_url(test_auth(), server.url());
        let err = client.send_message("C1", "hi").await.unwrap_err();
        match err {
            SlackError::Composio(msg) => assert!(msg.contains("not_in_channel")),
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
