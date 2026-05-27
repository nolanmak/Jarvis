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

    /// Fetch emails matching a Gmail search query (e.g. `from:jeremy@acme.com`,
    /// `subject:deadline after:2026/04/01`). Returns up to `limit` messages.
    async fn fetch_with_query(
        &self,
        entity_id: &str,
        query: &str,
        limit: u32,
    ) -> Result<Vec<Email>, GmailError>;

    /// Fetch sent-mail history for tone-mirroring backfill (#73).
    ///
    /// Wraps `in:sent` with an optional `after:<since_iso>` (YYYY/MM/DD)
    /// clause and walks pages up to `max_total` messages. Implementations
    /// SHOULD insert a small inter-page sleep to stay under Composio's
    /// observed ~5 req/s rate limit; the default `ComposioClient` impl uses
    /// 200ms per page.
    ///
    /// Default impl delegates to `fetch_with_query` so test fakes that
    /// override only one method still work; `ComposioClient` overrides this
    /// to lift the page cap from 10 → 25 (500 / 20).
    async fn fetch_sent_history(
        &self,
        entity_id: &str,
        since_iso: Option<&str>,
        max_total: u32,
    ) -> Result<Vec<Email>, GmailError> {
        let mut q = String::from("in:sent");
        if let Some(d) = since_iso {
            q.push_str(" after:");
            q.push_str(d);
        }
        self.fetch_with_query(entity_id, &q, max_total).await
    }

    /// Fetch the last `max` messages of a Gmail thread, oldest-first, for
    /// thread-aware drafting (#32).
    ///
    /// Wraps Gmail `users.threads.get`. Implementations SHOULD return the
    /// chronologically-ordered messages and leave token/char budgeting to the
    /// caller. The default impl returns an empty Vec so test fakes that don't
    /// model threads still compile; `ComposioClient` overrides it with a real
    /// `GMAIL_FETCH_MESSAGE_BY_THREAD_ID` call.
    async fn fetch_thread_messages(
        &self,
        _entity_id: &str,
        _thread_id: &str,
        _max: u32,
    ) -> Result<Vec<Email>, GmailError> {
        Ok(Vec::new())
    }

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

    /// Resolve the Gmail address for a connected account via Composio's
    /// `GMAIL_GET_PROFILE` (wraps Gmail API `users.getProfile`). Composio
    /// doesn't surface the address on the connection itself, so this lookup
    /// is the only way to label an entity by who it actually is. The address
    /// lands at `data.response_data.emailAddress`; `find_string_field` walks
    /// to it regardless of the exact nesting.
    pub async fn get_profile_email(&self, entity_id: &str) -> Result<String, GmailError> {
        let v = self
            .execute("GMAIL_GET_PROFILE", entity_id, serde_json::json!({}))
            .await?;
        find_string_field(&v, &["emailAddress", "email"]).ok_or_else(|| {
            GmailError::Decode(format!(
                "no emailAddress in GMAIL_GET_PROFILE response: {}",
                serde_json::to_string(&v).unwrap_or_default()
            ))
        })
    }
}

fn is_transient_reqwest(e: &reqwest::Error) -> bool {
    e.is_timeout() || e.is_connect() || e.is_request()
}

/// Extract the bare email address from an RFC 5322 header-style string.
/// `Name <x@y.com>` → `x@y.com`. Already-bare addresses pass through.
pub(crate) fn extract_bare_email(raw: &str) -> String {
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

    // ---- #164: tool-error propagation ----
    //
    // Regression test for the bug where the reasoner narrated a fabricated
    // "harness permission prompt" instead of surfacing the actual upstream
    // 401 from Composio. The propagation path under test is:
    //   ComposioClient::execute (formats `{action} → {status}: {text}`)
    //     → GmailError::Composio { message }
    //     → Display impl ("composio: {message}")
    // Anything above this layer (the CLI, anyhow chain, the reasoner's
    // tool-output capture) forwards the Display string verbatim. As long
    // as the response body lands in that string, the model has the real
    // error in context and is given clear-prompt instructions not to
    // editorialize it (see schema/wiki-ask.md, skills/email-triage/SKILL.md).
    //
    // We mock Composio with a one-shot tokio TCP listener that replies 401
    // with the same JSON shape the real revoked-key incident produced, then
    // assert the resulting GmailError::Composio Display string contains
    // both "401" and "Invalid API key". No actual model call is needed —
    // this tests the tool-error formatting layer the model relies on.

    use std::net::SocketAddr;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Spawn a single-response HTTP/1.1 mock that returns `body` with `status`
    /// to the first POST it sees, then closes. Returns the bound address.
    async fn spawn_one_shot_http(status: u16, body: &'static str) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            // Drain the request enough to know the client finished sending
            // headers. We only need the status line and headers; reqwest will
            // happily move on once the response is received.
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf).await;
            let resp = format!(
                "HTTP/1.1 {status} Unauthorized\r\nContent-Length: {len}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
                len = body.len(),
                body = body
            );
            let _ = socket.write_all(resp.as_bytes()).await;
            let _ = socket.shutdown().await;
        });
        addr
    }

    #[tokio::test]
    async fn composio_401_propagates_status_and_message_to_error_display() {
        let body =
            r#"{"error":{"message":"Invalid API key: ak_637S7*","code":10401,"type":"auth"}}"#;
        let addr = spawn_one_shot_http(401, body).await;
        let base = format!("http://{addr}");
        let client = ComposioClient::new("ak_637S7_fake".into()).with_base_url(base);

        let err = client
            .create_draft("entity-x", "to@example.com", "subj", "body", None)
            .await
            .expect_err("401 must surface as an error");
        let display = format!("{err}");

        // The reasoner-visible string must carry the real 401 signal so the
        // model cannot justifiably invent a "permission prompt" narrative.
        assert!(
            display.contains("401"),
            "missing 401 in error display: {display}"
        );
        assert!(
            display.contains("Invalid API key"),
            "missing upstream message in error display: {display}"
        );
        // And it should be tagged as a composio error so the model can route
        // it correctly (vs. e.g. a local file-read error).
        assert!(
            display.starts_with("composio:"),
            "missing composio tag in error display: {display}"
        );
        // The action name must be present so the operator can locate the
        // failing call.
        assert!(
            display.contains("GMAIL_CREATE_EMAIL_DRAFT"),
            "missing action in error display: {display}"
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
            platform: "gmail".into(),
            kind: "dm".into(),
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
        self.fetch_with_query(entity_id, "is:unread", max_total)
            .await
    }

    async fn fetch_with_query(
        &self,
        entity_id: &str,
        query: &str,
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
                "query": query,
                "max_results": this_page,
            });
            if let Some(tok) = &page_token {
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

    async fn fetch_sent_history(
        &self,
        entity_id: &str,
        since_iso: Option<&str>,
        max_total: u32,
    ) -> Result<Vec<Email>, GmailError> {
        // Same paginated loop as `fetch_with_query`, but with a higher page
        // cap so the backfill caller can pull up to ~500 messages in one
        // shot (25 pages × 20 per page). Inter-page sleep stays under the
        // observed ~5 req/s ceiling on Composio's GMAIL_FETCH_EMAILS path.
        const PAGE_SIZE: u32 = 20;
        const MAX_PAGES: u32 = 25;

        let mut query = String::from("in:sent");
        if let Some(d) = since_iso {
            query.push_str(" after:");
            query.push_str(d);
        }

        let mut collected: Vec<Email> = Vec::new();
        let mut page_token: Option<String> = None;

        for page in 0..MAX_PAGES {
            let want = (max_total as usize).saturating_sub(collected.len());
            if want == 0 {
                break;
            }
            let this_page = (want as u32).min(PAGE_SIZE);

            let mut args = serde_json::json!({
                "query": &query,
                "max_results": this_page,
            });
            if let Some(tok) = &page_token {
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
            // Be polite to Composio between pages — observed limit is ~5 req/s.
            // Skip the sleep on the last page to keep wallclock tight.
            if page + 1 < MAX_PAGES {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
        Ok(collected)
    }

    async fn fetch_thread_messages(
        &self,
        entity_id: &str,
        thread_id: &str,
        max: u32,
    ) -> Result<Vec<Email>, GmailError> {
        // Composio's GMAIL_FETCH_MESSAGE_BY_THREAD_ID returns every message in
        // the thread in one shot (a thread is small relative to the inbox, so
        // no pagination is needed). We trim to the last `max` here and let the
        // prompt layer apply the hard char cap.
        let args = serde_json::json!({
            "thread_id": thread_id,
            // Some Composio builds accept user_id inline; execute() also sets
            // it at the top level. Harmless duplicate; keeps older builds happy.
            "user_id": entity_id,
        });
        let v = self
            .execute("GMAIL_FETCH_MESSAGE_BY_THREAD_ID", entity_id, args)
            .await?;
        let parsed: FetchResp =
            serde_json::from_value(v).map_err(|e| GmailError::Decode(e.to_string()))?;
        let mut msgs: Vec<Email> = parsed
            .data
            .messages
            .into_iter()
            .filter_map(|m| m.into_email(entity_id))
            .collect();
        // Keep only the last `max` messages, preserving chronological order.
        if max > 0 && msgs.len() > max as usize {
            let drop = msgs.len() - max as usize;
            msgs.drain(0..drop);
        }
        Ok(msgs)
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
