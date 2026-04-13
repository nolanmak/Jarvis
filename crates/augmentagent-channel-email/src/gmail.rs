//! Gmail client over Composio HTTP (`/v3/actions/execute`).
//!
//! Thin wrapper: one function per Composio action we use. The channel adapter
//! depends on the `GmailApi` trait so Phase 1 tests can inject a fake.

use async_trait::async_trait;
use serde::Deserialize;
use thiserror::Error;

use augmentagent_store::Email;

#[derive(Debug, Error)]
pub enum GmailError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("composio: {message}")]
    Composio { message: String },
    #[error("decode: {0}")]
    Decode(String),
}

#[async_trait]
pub trait GmailApi: Send + Sync {
    /// Fetch unread emails for an entity. Returns up to `limit` messages.
    async fn fetch_unread(&self, entity_id: &str, limit: u32) -> Result<Vec<Email>, GmailError>;

    /// Create a reply draft. Returns the draft ID.
    async fn create_draft(
        &self,
        entity_id: &str,
        to: &str,
        subject: &str,
        body: &str,
        thread_id: Option<&str>,
    ) -> Result<String, GmailError>;

    /// Send an existing draft.
    async fn send_draft(&self, entity_id: &str, draft_id: &str) -> Result<(), GmailError>;
}

pub struct ComposioClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl ComposioClient {
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
    ) -> Result<serde_json::Value, GmailError> {
        let url = format!("{}/api/v3/actions/{}/execute", self.base_url, action);
        let body = serde_json::json!({
            "entityId": entity_id,
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
                    // Retry 5xx and 429; surface 4xx (other than 429) immediately.
                    let retryable = status.as_u16() == 429 || status.is_server_error();
                    let text = resp.text().await.unwrap_or_default();
                    let err = GmailError::Composio {
                        message: format!("{action} → {status}: {text}"),
                    };
                    if retryable && attempt < MAX_ATTEMPTS {
                        tracing::warn!(
                            action, status = %status, attempt, "composio retryable failure; backing off"
                        );
                        backoff(attempt).await;
                        continue;
                    }
                    return Err(err);
                }
                Err(e) if attempt < MAX_ATTEMPTS && is_transient_reqwest(&e) => {
                    tracing::warn!(action, attempt, "composio transport error; retrying: {e}");
                    backoff(attempt).await;
                    continue;
                }
                Err(e) => return Err(GmailError::Http(e)),
            }
        }
    }
}

fn is_transient_reqwest(e: &reqwest::Error) -> bool {
    e.is_timeout() || e.is_connect() || e.is_request()
}

async fn backoff(attempt: u32) {
    let base_ms: u64 = 300;
    let mult: u64 = 1 << attempt.min(5); // 2, 4, 8, ...
    let delay = std::time::Duration::from_millis(base_ms * mult);
    tokio::time::sleep(delay).await;
}

#[derive(Debug, Deserialize)]
struct FetchResp {
    #[serde(default)]
    data: FetchData,
}

#[derive(Debug, Default, Deserialize)]
struct FetchData {
    #[serde(default)]
    messages: Vec<FetchMessage>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FetchMessage {
    #[serde(alias = "id")]
    message_id: Option<String>,
    thread_id: Option<String>,
    from: Option<String>,
    sender: Option<String>,
    subject: Option<String>,
    #[serde(alias = "snippet", alias = "messageText")]
    body: Option<String>,
    date: Option<String>,
    received_time: Option<String>,
}

impl FetchMessage {
    fn into_email(self, account: &str) -> Option<Email> {
        let message_id = self.message_id?;
        Some(Email {
            message_id,
            thread_id: self.thread_id,
            from: self.from.or(self.sender).unwrap_or_default(),
            subject: self.subject.unwrap_or_default(),
            body: self.body.unwrap_or_default(),
            date: self.date.or(self.received_time).unwrap_or_default(),
            account_entity_id: Some(account.to_string()),
        })
    }
}

#[async_trait]
impl GmailApi for ComposioClient {
    async fn fetch_unread(&self, entity_id: &str, limit: u32) -> Result<Vec<Email>, GmailError> {
        let args = serde_json::json!({
            "query": "is:unread",
            "max_results": limit,
        });
        let v = self.execute("GMAIL_FETCH_EMAILS", entity_id, args).await?;
        let parsed: FetchResp = serde_json::from_value(v)
            .map_err(|e| GmailError::Decode(e.to_string()))?;
        Ok(parsed
            .data
            .messages
            .into_iter()
            .filter_map(|m| m.into_email(entity_id))
            .collect())
    }

    async fn create_draft(
        &self,
        entity_id: &str,
        to: &str,
        subject: &str,
        body: &str,
        thread_id: Option<&str>,
    ) -> Result<String, GmailError> {
        let mut args = serde_json::json!({
            "recipient_email": to,
            "subject": subject,
            "body": body,
        });
        if let Some(t) = thread_id {
            args["thread_id"] = serde_json::Value::String(t.to_string());
        }
        let v = self.execute("GMAIL_CREATE_DRAFT", entity_id, args).await?;
        v.get("data")
            .and_then(|d| d.get("draft_id").or_else(|| d.get("id")))
            .and_then(|s| s.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| GmailError::Decode("missing draft id".into()))
    }

    async fn send_draft(&self, entity_id: &str, draft_id: &str) -> Result<(), GmailError> {
        let args = serde_json::json!({ "draft_id": draft_id });
        self.execute("GMAIL_SEND_DRAFT", entity_id, args).await?;
        Ok(())
    }
}
