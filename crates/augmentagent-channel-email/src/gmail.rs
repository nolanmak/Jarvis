//! Gmail client over Composio HTTP (`/v3/actions/execute`).
//!
//! Thin wrapper: one function per Composio action we use. The channel adapter
//! depends on the `GmailApi` trait so Phase 1 tests can inject a fake.

use std::collections::HashSet;
use std::time::Duration;

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

    /// Shared paginated fetch for `GMAIL_FETCH_EMAILS`. Walks pages up to
    /// `max_total` messages (in `PAGE_SIZE` chunks), deduping by `message_id`
    /// and guarding against a non-advancing pagination cursor.
    ///
    /// Composio has been observed to echo the SAME `next_page_token` (or repeat
    /// the first message) across pages; without dedup + a token-advance guard
    /// the loop happily re-appends one message until it fills `max_total`,
    /// producing N identical rows (#331). We therefore keep a `seen` set and
    /// only collect new ids, and stop when there is no next token, the token
    /// did not change, or a non-empty page contributed zero new messages.
    ///
    /// `max_pages` caps the walk (10 for interactive search, 25 for the tone
    /// backfill); `sleep_between_pages`, when set, adds an inter-page delay to
    /// stay under Composio's observed ~5 req/s ceiling.
    async fn fetch_paged(
        &self,
        entity_id: &str,
        query: &str,
        max_total: u32,
        max_pages: u32,
        sleep_between_pages: Option<Duration>,
    ) -> Result<Vec<Email>, GmailError> {
        const PAGE_SIZE: u32 = 20;

        let mut collected: Vec<Email> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut page_token: Option<String> = None;

        for page in 0..max_pages {
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

            let next_token = parsed.data.next_page_token.clone();
            let msgs = parsed.data.messages;
            let returned = msgs.len();

            let mut new_this_page = 0usize;
            for m in msgs {
                if let Some(email) = m.into_email(entity_id) {
                    // Dedup by message id: a repeated message never gets added
                    // twice, no matter how many times Composio re-serves it.
                    if seen.insert(email.message_id.clone()) {
                        collected.push(email);
                        new_this_page += 1;
                        if collected.len() >= max_total as usize {
                            break;
                        }
                    }
                }
            }

            if collected.len() >= max_total as usize {
                break;
            }
            // No further pages.
            if next_token.is_none() {
                break;
            }
            // Cursor did not advance — Composio is re-serving the same page;
            // continuing would loop until we hit `max_pages` for nothing.
            if next_token == page_token {
                break;
            }
            // A non-empty page that yielded zero new messages means results are
            // repeating behind a churning token; stop rather than spin.
            if returned > 0 && new_this_page == 0 {
                break;
            }
            page_token = next_token;

            if let Some(delay) = sleep_between_pages {
                if page + 1 < max_pages {
                    tokio::time::sleep(delay).await;
                }
            }
        }

        // Cheap invariant: never return more than asked for.
        collected.truncate(max_total as usize);
        Ok(collected)
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

    /// A mock that replies with the SAME `body`/`status` to EVERY request it
    /// receives (each on a fresh `Connection: close` socket), so a paginated
    /// caller that keeps requesting sees the same page repeated — the Composio
    /// "stuck cursor" shape behind #331.
    async fn spawn_repeating_http(status: u16, body: &'static str) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = socket.read(&mut buf).await;
                    let resp = format!(
                        "HTTP/1.1 {status} OK\r\nContent-Length: {len}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
                        len = body.len(),
                        body = body
                    );
                    let _ = socket.write_all(resp.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        addr
    }

    // ---- #331: dedup + timestamp regressions ----
    //
    // Composio returned the same message (one messageId) across pages while
    // echoing a non-advancing next_page_token. The old loop blindly extended
    // its result vec, so a `--limit N` search came back as N identical rows
    // with empty date fields. This test reproduces that exact shape and
    // asserts: (1) the result is deduped to a single message, (2) the loop
    // terminates (token-advance guard, not the page cap), and (3) the
    // `messageTimestamp` field populates Email.date.
    #[tokio::test]
    async fn fetch_with_query_dedups_repeated_message_and_parses_timestamp() {
        let body = r#"{"data":{"messages":[{"id":"MSG1","from":"me@example.com","subject":"Invoice #35","messageTimestamp":"2026-01-17T21:11:09Z","messageText":"hi"}],"nextPageToken":"STUCK_CURSOR"}}"#;
        let addr = spawn_repeating_http(200, body).await;
        let base = format!("http://{addr}");
        let client = ComposioClient::new("ak_fake".into()).with_base_url(base);

        let emails = client
            .fetch_with_query("entity-x", "subject:Invoice #35 from:me", 3)
            .await
            .expect("search should succeed");

        // Bug 1: a stuck cursor re-serving one message must NOT fill the limit
        // with duplicates — exactly one distinct message comes back.
        assert_eq!(
            emails.len(),
            1,
            "expected dedup to a single message, got {} rows",
            emails.len()
        );
        assert_eq!(emails[0].message_id, "MSG1");
        // Bug 2: messageTimestamp must land in Email.date (was blank before).
        assert_eq!(emails[0].date, "2026-01-17T21:11:09Z");
        // And the coincidentally-covered fields still populate.
        assert_eq!(emails[0].subject, "Invoice #35");
        assert_eq!(emails[0].from, "me@example.com");
    }

    // #331 follow-up: some Composio builds emit `internalDate` as epoch-ms (a
    // JSON number). The timestamp field must accept that without hard-failing
    // the whole decode — the number is stringified into Email.date so the
    // downstream epoch-ms parser can use it.
    #[tokio::test]
    async fn fetch_with_query_accepts_numeric_internal_date() {
        let body = r#"{"data":{"messages":[{"id":"MSG9","from":"sender@example.com","subject":"Re: ping","internalDate":1737147069000,"messageText":"hi"}]}}"#;
        let addr = spawn_repeating_http(200, body).await;
        let base = format!("http://{addr}");
        let client = ComposioClient::new("ak_fake".into()).with_base_url(base);

        let emails = client
            .fetch_with_query("entity-x", "in:inbox", 1)
            .await
            .expect("a numeric internalDate must not fail the decode");

        assert_eq!(emails.len(), 1, "expected the message to parse, not error out");
        assert_eq!(emails[0].message_id, "MSG9");
        // The epoch-ms number is stringified into date (parse_date_ms handles it).
        assert_eq!(emails[0].date, "1737147069000");
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
    /// Composio's `GMAIL_FETCH_EMAILS` returns the message timestamp under
    /// `messageTimestamp` (RFC-3339, e.g. `2026-01-17T21:11:09Z`) — NOT under
    /// `date`/`receivedTime` — so the `date` column came back empty (#331).
    /// The `camelCase` rename already maps this field to `messageTimestamp`;
    /// the aliases also accept `internalDate`, which some Composio builds emit
    /// as **epoch-ms** — and epoch-ms is a JSON *number*. A plain
    /// `Option<String>` would hard-fail the entire response decode on a numeric
    /// `internalDate` (`invalid type: integer, expected a string`), so
    /// `de_num_or_string` accepts a string OR a number (stringifying the
    /// latter). Downstream `parse_date_ms` handles both RFC-3339 and epoch-ms.
    #[serde(
        default,
        alias = "internalDate",
        alias = "internal_date",
        deserialize_with = "de_num_or_string"
    )]
    message_timestamp: Option<String>,
}

/// Deserialize a field that Composio may send as either a JSON string
/// (`messageTimestamp` RFC-3339) or a JSON number (`internalDate` epoch-ms),
/// yielding `Option<String>`. Absent/null → `None`; a number is stringified so
/// it still reaches `Email.date` (and the epoch-ms branch of `parse_date_ms`).
/// Without this, a numeric value aborts the whole `FetchResp` decode (#331).
fn de_num_or_string<'de, D>(d: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum NumOrStr {
        S(String),
        I(i64),
        F(f64),
    }
    Ok(match Option::<NumOrStr>::deserialize(d)? {
        Some(NumOrStr::S(s)) => Some(s),
        Some(NumOrStr::I(n)) => Some(n.to_string()),
        Some(NumOrStr::F(f)) => Some((f as i64).to_string()),
        None => None,
    })
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
            // Prefer the real Composio field (`messageTimestamp`); fall back to
            // the legacy keys defensively. See the struct field doc for #331.
            date: self
                .message_timestamp
                .or(self.date)
                .or(self.received_time)
                .unwrap_or_default(),
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
        // so `fetch_paged` walks 20-email pages up to `max_total`. 10-page
        // safety cap, no inter-page delay for the interactive path.
        self.fetch_paged(entity_id, query, max_total, 10, None)
            .await
    }

    async fn fetch_sent_history(
        &self,
        entity_id: &str,
        since_iso: Option<&str>,
        max_total: u32,
    ) -> Result<Vec<Email>, GmailError> {
        let mut query = String::from("in:sent");
        if let Some(d) = since_iso {
            query.push_str(" after:");
            query.push_str(d);
        }
        // Higher page cap (25 = 500/20) so the backfill can pull ~500 messages
        // in one shot; a 200ms inter-page sleep stays under the observed
        // ~5 req/s ceiling on Composio's GMAIL_FETCH_EMAILS path.
        self.fetch_paged(
            entity_id,
            &query,
            max_total,
            25,
            Some(Duration::from_millis(200)),
        )
        .await
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
