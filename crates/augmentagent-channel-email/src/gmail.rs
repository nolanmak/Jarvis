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

    /// Replace the body of an existing draft (used by the revise flow).
    async fn update_draft(
        &self,
        entity_id: &str,
        draft_id: &str,
        to: &str,
        subject: &str,
        body: &str,
    ) -> Result<(), GmailError>;

    /// Send an existing draft.
    async fn send_draft(&self, entity_id: &str, draft_id: &str) -> Result<(), GmailError>;

    /// Delete an unsent draft from Gmail/Drafts. Used to clean up orphans
    /// after revise and to discard drafts the approver chose to skip.
    async fn delete_draft(&self, entity_id: &str, draft_id: &str) -> Result<(), GmailError>;
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

/// Extract the bare email address from an RFC 5322 header-style string.
/// `Name <x@y.com>` → `x@y.com`. Already-bare addresses pass through.
fn extract_bare_email(raw: &str) -> String {
    if let (Some(open), Some(close)) = (raw.find('<'), raw.rfind('>')) {
        if open < close {
            return raw[open + 1..close].trim().to_string();
        }
    }
    raw.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_bare_strips_display_name() {
        assert_eq!(extract_bare_email("Name <x@y.com>"), "x@y.com");
        assert_eq!(extract_bare_email("\"Quoted Name\" <x@y.com>"), "x@y.com");
    }

    #[test]
    fn extract_bare_passes_through_simple() {
        assert_eq!(extract_bare_email("x@y.com"), "x@y.com");
        assert_eq!(extract_bare_email("  x@y.com  "), "x@y.com");
    }

    #[test]
    fn extract_bare_handles_plus_addressing() {
        assert_eq!(
            extract_bare_email("User <user+tag@example.com>"),
            "user+tag@example.com"
        );
    }
}

/// Recursively search a JSON value for the first string-valued field whose
/// key matches any of `keys`. Used to tolerate Composio's variable response
/// shapes (sometimes nested under `data`, `data.response_data`, etc.).
fn find_string_field(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    match value {
        serde_json::Value::Object(map) => {
            for key in keys {
                if let Some(serde_json::Value::String(s)) = map.get(*key) {
                    if !s.is_empty() {
                        return Some(s.clone());
                    }
                }
            }
            for (_k, v) in map {
                if let Some(found) = find_string_field(v, keys) {
                    return Some(found);
                }
            }
            None
        }
        serde_json::Value::Array(arr) => {
            for v in arr {
                if let Some(found) = find_string_field(v, keys) {
                    return Some(found);
                }
            }
            None
        }
        _ => None,
    }
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
    /// Gmail's pagination cursor. Composio may serialize it under any of these keys.
    #[serde(
        default,
        alias = "next_page_token",
        alias = "nextPageToken",
        alias = "page_token",
        alias = "nextPage"
    )]
    next_page_token: Option<String>,
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
    async fn fetch_unread(
        &self,
        entity_id: &str,
        max_total: u32,
    ) -> Result<Vec<Email>, GmailError> {
        // Composio caps response payload size (seen 413 above ~40 KB of bodies),
        // so paginate in 20-email pages up to `max_total`.
        const PAGE_SIZE: u32 = 20;
        const MAX_PAGES: u32 = 10; // safety guard against runaway loops

        let mut collected: Vec<Email> = Vec::new();
        let mut page_token: Option<String> = None;

        for _page in 0..MAX_PAGES {
            let want = (max_total as usize).saturating_sub(collected.len());
            if want == 0 {
                break;
            }
            let this_page = (want as u32).min(PAGE_SIZE);

            let mut args = serde_json::json!({
                "query": "is:unread",
                "max_results": this_page,
            });
            if let Some(tok) = &page_token {
                // Gmail native param is `pageToken`; Composio passes it through.
                args["page_token"] = serde_json::Value::String(tok.clone());
            }

            let v = self.execute("GMAIL_FETCH_EMAILS", entity_id, args).await?;
            let parsed: FetchResp =
                serde_json::from_value(v).map_err(|e| GmailError::Decode(e.to_string()))?;

            let page_emails: Vec<Email> = parsed
                .data
                .messages
                .into_iter()
                .filter_map(|m| m.into_email(entity_id))
                .collect();

            if page_emails.is_empty() && parsed.data.next_page_token.is_none() {
                break;
            }

            collected.extend(page_emails);
            page_token = parsed.data.next_page_token;
            if page_token.is_none() {
                break;
            }
        }

        Ok(collected)
    }

    async fn create_draft(
        &self,
        entity_id: &str,
        to: &str,
        subject: &str,
        body: &str,
        thread_id: Option<&str>,
    ) -> Result<String, GmailError> {
        // Composio's GMAIL_CREATE_EMAIL_DRAFT expects a bare email address in
        // `recipient_email` — not the full RFC 5322 form with a display name.
        // Strip `Name <x@y.com>` → `x@y.com`. Leave already-bare addresses alone.
        let bare_to = extract_bare_email(to);
        let mut args = serde_json::json!({
            "recipient_email": bare_to,
            "subject": subject,
            "body": body,
        });
        if let Some(t) = thread_id {
            args["thread_id"] = serde_json::Value::String(t.to_string());
        }
        let v = self.execute("GMAIL_CREATE_EMAIL_DRAFT", entity_id, args).await?;
        // Composio response shapes vary across actions; recursively search for
        // any of the common draft-id key names.
        const DRAFT_ID_KEYS: &[&str] = &["draft_id", "draftId", "id"];
        if let Some(id) = find_string_field(&v, DRAFT_ID_KEYS) {
            return Ok(id);
        }
        Err(GmailError::Decode(format!(
            "missing draft id in response: {}",
            serde_json::to_string(&v).unwrap_or_default()
        )))
    }

    async fn update_draft(
        &self,
        entity_id: &str,
        draft_id: &str,
        to: &str,
        subject: &str,
        body: &str,
    ) -> Result<(), GmailError> {
        let args = serde_json::json!({
            "draft_id": draft_id,
            "recipient_email": extract_bare_email(to),
            "subject": subject,
            "body": body,
        });
        self.execute("GMAIL_UPDATE_DRAFT", entity_id, args).await?;
        Ok(())
    }

    async fn send_draft(&self, entity_id: &str, draft_id: &str) -> Result<(), GmailError> {
        let args = serde_json::json!({ "draft_id": draft_id });
        self.execute("GMAIL_SEND_DRAFT", entity_id, args).await?;
        Ok(())
    }

    async fn delete_draft(&self, entity_id: &str, draft_id: &str) -> Result<(), GmailError> {
        let args = serde_json::json!({ "draft_id": draft_id });
        self.execute("GMAIL_DELETE_DRAFT", entity_id, args).await?;
        Ok(())
    }
}
