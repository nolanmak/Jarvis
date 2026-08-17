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
    #[error("invalid input: {0}")]
    Invalid(String),
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

    /// Create a reply draft carrying Cc recipients (#629 reply-all). The
    /// default impl drops `cc` and delegates to [`create_draft`] so test
    /// fakes that don't model Cc keep compiling; `ComposioClient` overrides
    /// it to pass the list through `GMAIL_CREATE_EMAIL_DRAFT`'s `cc` param.
    async fn create_draft_with_cc(
        &self,
        entity_id: &str,
        to: &str,
        cc: &[String],
        subject: &str,
        body: &str,
        thread_id: Option<&str>,
    ) -> Result<String, GmailError> {
        let _ = cc;
        self.create_draft(entity_id, to, subject, body, thread_id)
            .await
    }

    /// Replace an existing draft's content. Returns the NEW draft id — the
    /// old id is invalid afterwards.
    ///
    /// Composio has no update-draft tool (`GMAIL_UPDATE_DRAFT` 404s with
    /// Tool_ToolNotFound — #382), so "update" is modeled as create-then-delete
    /// using the two tools that do exist. `thread_id` re-attaches the new
    /// draft to a thread; pass the old draft's thread to keep a reply
    /// threaded. Delete failure is non-fatal (the caller's goal — a draft
    /// with the new content — is met); the orphaned old draft is logged.
    async fn update_draft(
        &self,
        entity_id: &str,
        draft_id: &str,
        to: &str,
        subject: &str,
        body: &str,
        thread_id: Option<&str>,
    ) -> Result<String, GmailError> {
        let new_id = self
            .create_draft(entity_id, to, subject, body, thread_id)
            .await?;
        if let Err(e) = self.delete_draft(entity_id, draft_id).await {
            tracing::warn!(
                old_draft_id = draft_id,
                new_draft_id = %new_id,
                "update_draft: new draft created but old draft not deleted: {e}"
            );
        }
        Ok(new_id)
    }

    /// Send an existing draft. Returns the Gmail message id of the message
    /// that landed in SENT, when the provider reports one.
    ///
    /// #449: the id is what lets the OutboundObserver tell the daemon's own
    /// replies apart from replies the user typed in Gmail web/mobile. Callers
    /// MUST hand it to `Store::record_self_sent_message`. `None` means the
    /// provider didn't return an id — the send still succeeded, and the
    /// observer falls back to `Store::has_daemon_send_near`.
    async fn send_draft(
        &self,
        entity_id: &str,
        draft_id: &str,
    ) -> Result<Option<String>, GmailError>;

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
                        let v = resp.json::<serde_json::Value>().await?;
                        return check_envelope(action, v);
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

    /// Resolve a Gmail id — message id OR thread id — to the thread id it
    /// belongs to, scoped to one account's mailbox (#381).
    ///
    /// `GMAIL_CREATE_EMAIL_DRAFT`'s `thread_id` must be a real Gmail thread
    /// id; a message id 404s ("Requested entity was not found"). Callers
    /// usually hold a messageId from `gmail search`, so we accept either:
    ///
    /// 1. Try `users.messages.get` (format=minimal — tiny payload). A message
    ///    id resolves to its `threadId`; a thread-HEAD id also succeeds
    ///    (Gmail thread ids equal the first message's id) and maps to itself.
    /// 2. Fall back to `users.threads.get` for the rare valid thread id whose
    ///    head message was deleted.
    ///
    /// Both failing means the id isn't in THIS account's mailbox at all —
    /// commonly the thread lives in a different connected account.
    pub async fn resolve_thread_id(
        &self,
        entity_id: &str,
        id: &str,
    ) -> Result<String, GmailError> {
        // NOTE: no inline `user_id` in the arguments. The Gmail tools forward
        // it as the Gmail API `users/<user_id>/…` path segment, where only
        // 'me' or an email address is valid — a Composio entity id 403s with
        // "Delegation denied". Account routing is the execute() envelope's job.
        let args = serde_json::json!({
            "message_id": id,
            "format": "minimal",
        });
        // execute() rejects successful:false envelopes centrally (#402), so
        // an Ok here is a real hit.
        if let Ok(v) = self
            .execute("GMAIL_FETCH_MESSAGE_BY_MESSAGE_ID", entity_id, args)
            .await
        {
            if let Some(t) = find_string_field(&v, &["threadId", "thread_id"]) {
                return Ok(t);
            }
        }
        match self.fetch_thread_messages(entity_id, id, 1).await {
            Ok(msgs) if !msgs.is_empty() => Ok(id.to_string()),
            _ => Err(GmailError::Composio {
                message: format!(
                    "id {id} is neither a message nor a thread in this account's mailbox"
                ),
            }),
        }
    }

    /// Look up the thread an existing draft is attached to, via
    /// `GMAIL_LIST_DRAFTS` (there is no get-draft tool in the Composio
    /// catalog). Returns `Ok(None)` when the draft is standalone — Gmail
    /// gives an unthreaded draft its own thread (threadId == message id),
    /// and recreating against that thread would 404 once the draft is
    /// deleted — or when the draft isn't in the first `MAX_PAGES` pages.
    pub async fn get_draft_thread_id(
        &self,
        entity_id: &str,
        draft_id: &str,
    ) -> Result<Option<String>, GmailError> {
        const MAX_PAGES: u32 = 5;
        let mut page_token: Option<String> = None;
        for _ in 0..MAX_PAGES {
            // No inline user_id — see resolve_thread_id for why.
            let mut args = serde_json::json!({
                "max_results": 100,
                "verbose": false,
            });
            if let Some(t) = &page_token {
                args["page_token"] = serde_json::Value::String(t.clone());
            }
            let v = self.execute("GMAIL_LIST_DRAFTS", entity_id, args).await?;
            let drafts = v
                .pointer("/data/drafts")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default();
            for d in &drafts {
                if d.get("id").and_then(serde_json::Value::as_str) != Some(draft_id) {
                    continue;
                }
                let msg_id = d
                    .pointer("/message/id")
                    .and_then(serde_json::Value::as_str);
                let thread_id = d
                    .pointer("/message/threadId")
                    .or_else(|| d.pointer("/message/thread_id"))
                    .and_then(serde_json::Value::as_str);
                return Ok(match (msg_id, thread_id) {
                    (Some(m), Some(t)) if m != t => Some(t.to_string()),
                    _ => None,
                });
            }
            page_token = v
                .pointer("/data/next_page_token")
                .or_else(|| v.pointer("/data/nextPageToken"))
                .and_then(serde_json::Value::as_str)
                .map(String::from);
            if page_token.is_none() {
                break;
            }
        }
        Ok(None)
    }

    /// #417 — Upload a local file into Composio's attachment store and return
    /// the `{name, mimetype, s3key}` triple the Gmail tools take as their
    /// `attachment` argument.
    ///
    /// Flow (probed live 2026-07-09): POST `/api/v3/files/upload/request`
    /// with `{toolkit_slug, tool_slug, tool_input_field: "attachment",
    /// filename, mimetype, md5}` → `{key, new_presigned_url}`; PUT the raw
    /// bytes to the presigned URL (Content-Type must match the one signed
    /// into the URL); pass `key` as `s3key`. When the same md5 was uploaded
    /// before, the response can omit `new_presigned_url` — the object
    /// already exists and `key` is reusable as-is.
    pub async fn upload_attachment(
        &self,
        tool_slug: &str,
        path: &std::path::Path,
    ) -> Result<Attachment, GmailError> {
        use md5::{Digest, Md5};

        let bytes = std::fs::read(path).map_err(|e| {
            GmailError::Decode(format!("read attachment {}: {e}", path.display()))
        })?;
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("attachment")
            .to_string();
        let mimetype = guess_mimetype(&name).to_string();
        let md5_hex = format!("{:x}", Md5::digest(&bytes));

        let url = format!("{}/api/v3/files/upload/request", self.base_url);
        let req = serde_json::json!({
            "toolkit_slug": "gmail",
            "tool_slug": tool_slug,
            "tool_input_field": "attachment",
            "filename": name,
            "mimetype": mimetype,
            "md5": md5_hex,
        });
        let resp = self
            .http
            .post(&url)
            .header("x-api-key", &self.api_key)
            .json(&req)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(GmailError::Composio {
                message: format!("files/upload/request → {status}: {text}"),
            });
        }
        let v: serde_json::Value = resp.json().await?;
        let key = v
            .get("key")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                GmailError::Decode(format!(
                    "no key in upload response: {}",
                    serde_json::to_string(&v).unwrap_or_default()
                ))
            })?
            .to_string();
        if let Some(put_url) = v.get("new_presigned_url").and_then(serde_json::Value::as_str) {
            let put = self
                .http
                .put(put_url)
                .header("content-type", &mimetype)
                .body(bytes)
                .send()
                .await?;
            if !put.status().is_success() {
                let s = put.status();
                let t = put.text().await.unwrap_or_default();
                return Err(GmailError::Composio {
                    message: format!("attachment upload PUT → {s}: {t}"),
                });
            }
        }
        Ok(Attachment {
            name,
            mimetype,
            s3key: key,
        })
    }

    /// `create_draft` with an optional uploaded attachment (#417) and
    /// optional cc/bcc lists (#439). Inherent (not on the `GmailApi` trait)
    /// so test fakes and the triage channel — which never attach or cc —
    /// stay untouched.
    ///
    /// `to` may carry SEVERAL addresses (`a@x.com, b@y.com`, display names
    /// allowed). Composio's `GMAIL_CREATE_EMAIL_DRAFT` takes exactly one
    /// `recipient_email` string plus an `extra_recipients` array — a
    /// comma-joined string is rejected as "Invalid email format" (#439) —
    /// so the split lives HERE rather than at each caller: the Revise
    /// redraft path round-trips the joined list through `emails.from` and
    /// must keep everyone on it.
    pub async fn create_draft_with_attachment(
        &self,
        entity_id: &str,
        to: &str,
        subject: &str,
        body: &str,
        thread_id: Option<&str>,
        attachment: Option<&Attachment>,
        cc: &[String],
        bcc: &[String],
    ) -> Result<String, GmailError> {
        let recipients = split_recipients(to);
        let (first, rest) = recipients
            .split_first()
            .ok_or_else(|| GmailError::Invalid(format!("no recipient address in {to:?}")))?;
        let mut args = serde_json::json!({
            "recipient_email": first,
            "subject": subject,
            "body": body,
        });
        if !rest.is_empty() {
            args["extra_recipients"] = serde_json::json!(rest);
        }
        let cc: Vec<String> = cc.iter().flat_map(|s| split_recipients(s)).collect();
        if !cc.is_empty() {
            args["cc"] = serde_json::json!(cc);
        }
        let bcc: Vec<String> = bcc.iter().flat_map(|s| split_recipients(s)).collect();
        if !bcc.is_empty() {
            args["bcc"] = serde_json::json!(bcc);
        }
        if let Some(t) = thread_id {
            args["thread_id"] = serde_json::Value::String(t.to_string());
        }
        if let Some(att) = attachment {
            args["attachment"] = serde_json::json!({
                "name": att.name,
                "mimetype": att.mimetype,
                "s3key": att.s3key,
            });
        }
        let v = self.execute("GMAIL_CREATE_EMAIL_DRAFT", entity_id, args).await?;
        const DRAFT_ID_KEYS: &[&str] = &["draft_id", "draftId", "id"];
        if let Some(id) = find_string_field(&v, DRAFT_ID_KEYS) {
            return Ok(id);
        }
        Err(GmailError::Decode(format!(
            "missing draft id in response: {}",
            serde_json::to_string(&v).unwrap_or_default()
        )))
    }

    /// `update_draft` (create replacement + delete old) with an optional
    /// attachment (#417) and cc/bcc lists (#439) for the replacement.
    /// Mirrors the trait default; returns the NEW draft id.
    #[allow(clippy::too_many_arguments)]
    pub async fn update_draft_with_attachment(
        &self,
        entity_id: &str,
        draft_id: &str,
        to: &str,
        subject: &str,
        body: &str,
        thread_id: Option<&str>,
        attachment: Option<&Attachment>,
        cc: &[String],
        bcc: &[String],
    ) -> Result<String, GmailError> {
        let new_id = self
            .create_draft_with_attachment(
                entity_id, to, subject, body, thread_id, attachment, cc, bcc,
            )
            .await?;
        if let Err(e) = self.delete_draft(entity_id, draft_id).await {
            tracing::warn!(
                old_draft_id = draft_id,
                new_draft_id = %new_id,
                "update_draft: new draft created but old draft not deleted: {e}"
            );
        }
        Ok(new_id)
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

/// An uploaded Composio attachment, ready to pass to the Gmail draft/send
/// tools as their `attachment` argument (#417).
#[derive(Debug, Clone)]
pub struct Attachment {
    pub name: String,
    pub mimetype: String,
    pub s3key: String,
}

/// Best-effort mimetype from the filename extension. Composio requires one
/// for the presigned upload; unknown extensions fall back to octet-stream
/// (Gmail still attaches them fine).
pub fn guess_mimetype(filename: &str) -> &'static str {
    let ext = filename.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "pdf" => "application/pdf",
        "txt" | "text" | "log" => "text/plain",
        "md" => "text/markdown",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "json" => "application/json",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

/// Extract the bare email address from an RFC 5322 header-style string.
/// `Name <x@y.com>` → `x@y.com`. Already-bare addresses pass through.
pub fn extract_bare_email(raw: &str) -> String {
    if let (Some(open), Some(close)) = (raw.find('<'), raw.rfind('>')) {
        if open < close {
            return raw[open + 1..close].trim().to_string();
        }
    }
    raw.trim().to_string()
}

/// Split an RFC 5322-style recipient list into bare addresses (#439).
/// Commas inside double quotes (`"Doe, John" <j@x.com>`) or angle brackets
/// don't split; each part is then reduced via [`extract_bare_email`] and
/// empty parts are dropped.
pub fn split_recipients(raw: &str) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut in_angle = false;
    for c in raw.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                cur.push(c);
            }
            '<' if !in_quotes => {
                in_angle = true;
                cur.push(c);
            }
            '>' if !in_quotes => {
                in_angle = false;
                cur.push(c);
            }
            ',' if !in_quotes && !in_angle => parts.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    parts.push(cur);
    parts
        .iter()
        .map(|p| extract_bare_email(p))
        .filter(|p| !p.is_empty())
        .collect()
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

    // ---- #439: multi-recipient splitting ----

    #[test]
    fn split_recipients_comma_list() {
        assert_eq!(
            split_recipients("nayra@example.com,bo@example.com"),
            vec!["nayra@example.com", "bo@example.com"]
        );
        // The space-after-comma variant from the second failure log.
        assert_eq!(
            split_recipients("nayra@example.com, bo@example.com"),
            vec!["nayra@example.com", "bo@example.com"]
        );
    }

    #[test]
    fn split_recipients_single_passthrough() {
        assert_eq!(split_recipients("x@y.com"), vec!["x@y.com"]);
        assert_eq!(split_recipients("Name <x@y.com>"), vec!["x@y.com"]);
    }

    #[test]
    fn split_recipients_display_names_with_quoted_comma() {
        assert_eq!(
            split_recipients(r#""Doe, John" <j@x.com>, Jane <jane@y.com>"#),
            vec!["j@x.com", "jane@y.com"]
        );
    }

    #[test]
    fn split_recipients_drops_empty_parts() {
        assert_eq!(split_recipients("a@x.com,, b@y.com,"), vec!["a@x.com", "b@y.com"]);
        assert!(split_recipients("").is_empty());
        assert!(split_recipients(" , ").is_empty());
    }

    // ---- #629: To/Cc recovery from the fetch payload ----
    //
    // Composio returns the full To list top-level and the Cc only inside
    // `payload.headers` (probed live 2026-08-17; the header name has been
    // seen as both `Cc` and `CC`). Reply-all depends on both landing on
    // `Email`.

    #[test]
    fn fetch_message_maps_top_level_to_and_cc_header() {
        let v = serde_json::json!({
            "messageId": "m1",
            "threadId": "t1",
            "sender": "a@example.com",
            "messageText": "hi",
            "messageTimestamp": "2026-08-17T00:00:00Z",
            "to": "Ann <ann@example.com>, bo@example.com",
            "payload": {"headers": [
                {"name": "Subject", "value": "irrelevant"},
                {"name": "CC", "value": "\"cee@example.com\" <cee@example.com>, Dee <dee@example.com>"}
            ]}
        });
        let m: super::FetchMessage = serde_json::from_value(v).unwrap();
        let e = m.into_email("acct").unwrap();
        assert_eq!(e.to, "Ann <ann@example.com>, bo@example.com");
        assert_eq!(
            e.cc,
            "\"cee@example.com\" <cee@example.com>, Dee <dee@example.com>"
        );
    }

    #[test]
    fn fetch_message_falls_back_to_the_to_header_and_tolerates_absent_payload() {
        // No top-level `to` → dig it out of the raw headers.
        let v = serde_json::json!({
            "messageId": "m2",
            "sender": "a@example.com",
            "payload": {"headers": [{"name": "To", "value": "b@example.com"}]}
        });
        let m: super::FetchMessage = serde_json::from_value(v).unwrap();
        let e = m.into_email("acct").unwrap();
        assert_eq!(e.to, "b@example.com");
        assert_eq!(e.cc, "");

        // No payload at all (some Composio responses omit it) → empty, not
        // a decode failure.
        let v = serde_json::json!({"messageId": "m3", "sender": "a@example.com"});
        let m: super::FetchMessage = serde_json::from_value(v).unwrap();
        let e = m.into_email("acct").unwrap();
        assert_eq!(e.to, "");
        assert_eq!(e.cc, "");
    }

    #[test]
    fn fetch_message_survives_a_shape_shifted_payload() {
        // #331 lesson: Composio fields drift shape between builds. A payload
        // that is a string / number / oddly-typed headers list must degrade
        // to "no cc", never abort the decode (which would drop every email
        // in the page).
        for bad_payload in [
            serde_json::json!("not an object"),
            serde_json::json!(42),
            serde_json::json!({"headers": "nope"}),
            serde_json::json!({"headers": [{"name": 5}]}),
            serde_json::Value::Null,
        ] {
            let v = serde_json::json!({
                "messageId": "m4",
                "sender": "a@example.com",
                "to": "b@example.com",
                "payload": bad_payload,
            });
            let m: super::FetchMessage =
                serde_json::from_value(v).expect("lenient payload must not fail decode");
            let e = m.into_email("acct").unwrap();
            assert_eq!(e.to, "b@example.com");
            assert_eq!(e.cc, "");
        }
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

    /// One-shot mock that CAPTURES the request (headers + body) and sends it
    /// down the returned channel, so a test can assert on the exact JSON the
    /// client put on the wire (#439). Replies 200 with `body`.
    async fn spawn_capturing_http(
        body: &'static str,
    ) -> (SocketAddr, tokio::sync::oneshot::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            // Read until the headers are complete AND Content-Length bytes of
            // body have arrived — a single read can return a partial request.
            let mut raw: Vec<u8> = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let Ok(n) = socket.read(&mut buf).await else { break };
                if n == 0 {
                    break;
                }
                raw.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&raw);
                if let Some(header_end) = text.find("\r\n\r\n") {
                    let content_len = text
                        .lines()
                        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(str::trim).map(str::to_string))
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(0);
                    if raw.len() >= header_end + 4 + content_len {
                        break;
                    }
                }
            }
            let _ = tx.send(String::from_utf8_lossy(&raw).into_owned());
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {len}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{body}",
                len = body.len(),
            );
            let _ = socket.write_all(resp.as_bytes()).await;
            let _ = socket.shutdown().await;
        });
        (addr, rx)
    }

    // ---- #439: multi-recipient / cc / bcc land as the array params Composio
    // actually accepts, not a comma-joined `recipient_email` string ----

    #[tokio::test]
    async fn create_draft_splits_to_into_recipient_email_and_extra_recipients() {
        let resp = r#"{"successful":true,"data":{"response_data":{"id":"DRAFT439"}}}"#;
        let (addr, captured) = spawn_capturing_http(resp).await;
        let client = ComposioClient::new("ak_fake".into()).with_base_url(format!("http://{addr}"));

        let id = client
            .create_draft_with_attachment(
                "entity-x",
                "nayra@example.com, Bo <bo@example.com>",
                "subj",
                "body",
                None,
                None,
                &["cc1@x.com,cc2@y.com".into()],
                &["hidden@z.com".into()],
            )
            .await
            .expect("multi-recipient draft must succeed");
        assert_eq!(id, "DRAFT439");

        let raw = captured.await.expect("request captured");
        let json_start = raw.find("\r\n\r\n").expect("has body") + 4;
        let v: serde_json::Value = serde_json::from_str(&raw[json_start..]).expect("json body");
        let args = &v["arguments"];
        assert_eq!(args["recipient_email"], "nayra@example.com");
        assert_eq!(args["extra_recipients"], serde_json::json!(["bo@example.com"]));
        assert_eq!(args["cc"], serde_json::json!(["cc1@x.com", "cc2@y.com"]));
        assert_eq!(args["bcc"], serde_json::json!(["hidden@z.com"]));
    }

    #[tokio::test]
    async fn create_draft_single_recipient_omits_array_params() {
        let resp = r#"{"successful":true,"data":{"response_data":{"id":"DRAFT1"}}}"#;
        let (addr, captured) = spawn_capturing_http(resp).await;
        let client = ComposioClient::new("ak_fake".into()).with_base_url(format!("http://{addr}"));

        client
            .create_draft("entity-x", "solo@x.com", "s", "b", None)
            .await
            .expect("single-recipient draft");

        let raw = captured.await.expect("request captured");
        let json_start = raw.find("\r\n\r\n").expect("has body") + 4;
        let v: serde_json::Value = serde_json::from_str(&raw[json_start..]).expect("json body");
        let args = &v["arguments"];
        assert_eq!(args["recipient_email"], "solo@x.com");
        // Absent — Composio treats empty arrays fine, but the pre-#439 wire
        // shape (no extra keys) is preserved for the single case.
        assert!(args.get("extra_recipients").is_none());
        assert!(args.get("cc").is_none());
        assert!(args.get("bcc").is_none());
    }

    #[tokio::test]
    async fn create_draft_rejects_empty_recipient_list() {
        let client = ComposioClient::new("ak_fake".into());
        let err = client
            .create_draft("entity-x", " , ", "s", "b", None)
            .await
            .expect_err("empty recipient list must fail client-side");
        assert!(matches!(err, GmailError::Invalid(_)), "got: {err}");
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

    // ---- #381: --thread-id resolution ----

    // A messageId (the only id `gmail search` used to print) must resolve to
    // the thread it belongs to via users.messages.get, so compose can accept
    // either id form.
    #[tokio::test]
    async fn resolve_thread_id_maps_message_id_to_thread() {
        let body = r#"{"successful":true,"data":{"id":"MSG2","threadId":"THREAD1","labelIds":["INBOX"]}}"#;
        let addr = spawn_one_shot_http(200, body).await;
        let client = ComposioClient::new("ak_fake".into()).with_base_url(format!("http://{addr}"));

        let t = client
            .resolve_thread_id("entity-x", "MSG2")
            .await
            .expect("message id should resolve");
        assert_eq!(t, "THREAD1");
    }

    // An id that is neither a message nor a thread in this mailbox (the
    // wrong-account case behind #381) must fail with an actionable error,
    // not a raw Gmail 404.
    #[tokio::test]
    async fn resolve_thread_id_rejects_unknown_id() {
        // Served for BOTH the messages.get and threads.get attempts.
        let body = r#"{"successful":false,"error":"Requested entity was not found.","data":{}}"#;
        let addr = spawn_repeating_http(200, body).await;
        let client = ComposioClient::new("ak_fake".into()).with_base_url(format!("http://{addr}"));

        let err = client
            .resolve_thread_id("entity-x", "BOGUS")
            .await
            .expect_err("unknown id must not resolve");
        let display = format!("{err}");
        assert!(
            display.contains("neither a message nor a thread"),
            "unhelpful error: {display}"
        );
    }

    // ---- #382: update_draft = create new + delete old ----

    // GMAIL_UPDATE_DRAFT doesn't exist in the Composio catalog, so the
    // default impl recreates the draft. The caller must get the NEW id back.
    #[tokio::test]
    async fn update_draft_recreates_and_returns_new_id() {
        // Same body serves both the create (draft id lookup) and the delete.
        let body = r#"{"successful":true,"data":{"id":"NEWDRAFT"}}"#;
        let addr = spawn_repeating_http(200, body).await;
        let client = ComposioClient::new("ak_fake".into()).with_base_url(format!("http://{addr}"));

        let new_id = client
            .update_draft("entity-x", "OLDDRAFT", "to@example.com", "subj", "body", None)
            .await
            .expect("update via recreate should succeed");
        assert_eq!(new_id, "NEWDRAFT");
    }

    // ---- #417: attachment upload flow ----

    // upload_attachment: request a presigned URL (mock A), PUT the bytes to
    // it (mock B), return {name, mimetype, s3key} with the server's key.
    #[tokio::test]
    async fn upload_attachment_requests_url_puts_bytes_and_returns_key() {
        // Mock B accepts the PUT.
        let put_addr = spawn_one_shot_http(200, "").await;
        // Mock A hands out the presigned URL pointing at mock B. Body must be
        // built at runtime (addr), so spawn_repeating_http can't be used with
        // a static str — inline a one-shot with a leaked String instead.
        let body: &'static str = Box::leak(
            format!(
                r#"{{"id":"up_1","key":"317291/gmail/GMAIL_CREATE_EMAIL_DRAFT/request/abc","new_presigned_url":"http://{put_addr}/put"}}"#
            )
            .into_boxed_str(),
        );
        let req_addr = spawn_one_shot_http(200, body).await;
        let client =
            ComposioClient::new("ak_fake".into()).with_base_url(format!("http://{req_addr}"));

        let tmp = tempfile::NamedTempFile::with_suffix(".pdf").unwrap();
        std::fs::write(tmp.path(), b"%PDF-1.4 fake resume bytes").unwrap();

        let att = client
            .upload_attachment("GMAIL_CREATE_EMAIL_DRAFT", tmp.path())
            .await
            .expect("upload should succeed");
        assert!(att.name.ends_with(".pdf"));
        assert_eq!(att.mimetype, "application/pdf");
        assert_eq!(att.s3key, "317291/gmail/GMAIL_CREATE_EMAIL_DRAFT/request/abc");
    }

    // Re-upload of an already-known file: response carries the key but no
    // presigned URL — must succeed without attempting a PUT.
    #[tokio::test]
    async fn upload_attachment_reuses_existing_object_without_put() {
        let body = r#"{"id":"up_2","key":"existing/key/xyz"}"#;
        let addr = spawn_one_shot_http(200, body).await;
        let client = ComposioClient::new("ak_fake".into()).with_base_url(format!("http://{addr}"));

        let tmp = tempfile::NamedTempFile::with_suffix(".txt").unwrap();
        std::fs::write(tmp.path(), b"hello").unwrap();

        let att = client
            .upload_attachment("GMAIL_CREATE_EMAIL_DRAFT", tmp.path())
            .await
            .expect("existing object should be reused");
        assert_eq!(att.s3key, "existing/key/xyz");
        assert_eq!(att.mimetype, "text/plain");
    }

    #[test]
    fn mimetype_guesses_cover_common_files() {
        assert_eq!(guess_mimetype("resume.pdf"), "application/pdf");
        assert_eq!(guess_mimetype("notes.MD"), "text/markdown");
        assert_eq!(guess_mimetype("photo.JPG"), "image/jpeg");
        assert_eq!(guess_mimetype("weird.bin"), "application/octet-stream");
        assert_eq!(guess_mimetype("noext"), "application/octet-stream");
    }

    // ---- #402: HTTP 200 + successful:false must be an error, not success ----

    // delete_draft was the live failure: `deleted:` printed while the draft
    // stayed in the mailbox. The envelope error text and log_id must surface.
    #[tokio::test]
    async fn successful_false_envelope_fails_delete_draft() {
        let body = r#"{"successful":false,"error":"Invalid delete request","log_id":"log_abc123","data":{}}"#;
        let addr = spawn_one_shot_http(200, body).await;
        let client = ComposioClient::new("ak_fake".into()).with_base_url(format!("http://{addr}"));

        let err = client
            .delete_draft("entity-x", "19f42a66139fbe45")
            .await
            .expect_err("successful:false must not report success");
        let display = format!("{err}");
        assert!(display.contains("successful:false"), "missing marker: {display}");
        assert!(display.contains("Invalid delete request"), "missing upstream error: {display}");
        assert!(display.contains("log_abc123"), "missing log_id: {display}");
        assert!(display.contains("GMAIL_DELETE_DRAFT"), "missing action: {display}");
    }

    // send_draft is the scariest variant: a false success here marks an
    // approval action Sent while no mail went out.
    #[tokio::test]
    async fn successful_false_envelope_fails_send_draft() {
        let body = r#"{"successful":false,"error":"Requested entity was not found.","data":{}}"#;
        let addr = spawn_one_shot_http(200, body).await;
        let client = ComposioClient::new("ak_fake".into()).with_base_url(format!("http://{addr}"));

        assert!(
            client.send_draft("entity-x", "r-nonexistent").await.is_err(),
            "send_draft must fail on successful:false"
        );
    }

    // Leniency guard: envelopes WITHOUT the `successful` key (older builds,
    // fetch shapes) must keep working — only an explicit false fails.
    #[tokio::test]
    async fn envelope_without_successful_key_still_succeeds() {
        let body = r#"{"data":{"id":"DRAFT_OK"}}"#;
        let addr = spawn_one_shot_http(200, body).await;
        let client = ComposioClient::new("ak_fake".into()).with_base_url(format!("http://{addr}"));

        let id = client
            .create_draft("entity-x", "to@example.com", "subj", "body", None)
            .await
            .expect("missing successful key must not fail");
        assert_eq!(id, "DRAFT_OK");
    }

    // Thread preservation source for update-draft: a threaded draft reports
    // its thread; a standalone draft (its message IS the thread head) must
    // report None, because recreating against a thread that only contained
    // the deleted draft 404s.
    #[tokio::test]
    async fn get_draft_thread_id_distinguishes_threaded_from_standalone() {
        let body = r#"{"successful":true,"data":{"drafts":[
            {"id":"D1","message":{"id":"M1","threadId":"T9"}},
            {"id":"D2","message":{"id":"M2","threadId":"M2"}}
        ]}}"#;
        let addr = spawn_repeating_http(200, body).await;
        let client = ComposioClient::new("ak_fake".into()).with_base_url(format!("http://{addr}"));

        let threaded = client
            .get_draft_thread_id("entity-x", "D1")
            .await
            .expect("list should succeed");
        assert_eq!(threaded.as_deref(), Some("T9"));

        let standalone = client
            .get_draft_thread_id("entity-x", "D2")
            .await
            .expect("list should succeed");
        assert_eq!(standalone, None);
    }
}

/// Composio v3 wraps TOOL-level failures in an HTTP 200 envelope:
/// `{"successful": false, "error": "…", "log_id": "…"}`. Treating 2xx as
/// success made every void operation (delete_draft, send_draft) report
/// false success — `delete-draft` printed `deleted:` while the draft stayed
/// live in the mailbox, and an Approve could claim "sent" having sent
/// nothing (#402, observed live 2026-07-08). Fail here, centrally, so every
/// caller sees the truth.
///
/// Lenient on shape: only an explicit `"successful": false` (or a non-null
/// string `"error"` alongside it) fails; envelopes without the key (older
/// builds, mock tests) pass through untouched.
fn check_envelope(
    action: &str,
    v: serde_json::Value,
) -> Result<serde_json::Value, GmailError> {
    let unsuccessful = v.get("successful").and_then(serde_json::Value::as_bool) == Some(false);
    if !unsuccessful {
        return Ok(v);
    }
    let err_text = v
        .get("error")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("(no error field in envelope)");
    let log_id = v
        .get("log_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("-");
    Err(GmailError::Composio {
        message: format!(
            "{action} reported successful:false — {} (composio log_id {log_id})",
            err_text.trim()
        ),
    })
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
    /// Full `To:` recipient list, comma-joined with display names — Composio
    /// surfaces it top-level on every GMAIL_FETCH_EMAILS /
    /// GMAIL_FETCH_MESSAGE_BY_THREAD_ID message (probed live 2026-08-17).
    to: Option<String>,
    /// Raw RFC-822 headers under `payload.headers`. `Cc:` has no top-level
    /// field, so it must be dug out of here (#629). Deserialized leniently —
    /// an absent, null, or shape-shifted `payload` degrades to `None` (empty
    /// cc) instead of hard-failing the whole fetch decode, per the #331
    /// lesson on Composio response drift.
    #[serde(default, deserialize_with = "de_lenient_payload")]
    payload: Option<FetchPayload>,
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

#[derive(Debug, Default, Deserialize)]
struct FetchPayload {
    #[serde(default)]
    headers: Vec<FetchHeader>,
}

/// Accept `payload` in any shape: a well-formed object parses, anything else
/// (string, number, array, headers items of the wrong type) yields `None`
/// rather than aborting the entire `FetchResp` decode — losing every email in
/// the page over one malformed field is exactly the #331 failure mode.
fn de_lenient_payload<'de, D>(d: D) -> Result<Option<FetchPayload>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(d)?;
    Ok(serde_json::from_value(v).ok())
}

#[derive(Debug, Deserialize)]
struct FetchHeader {
    #[serde(default)]
    name: String,
    #[serde(default)]
    value: String,
}

impl FetchMessage {
    /// The value of the first raw header matching `name`, case-insensitively —
    /// Gmail emits `Cc`, Composio has been seen relaying `CC`.
    fn header(&self, name: &str) -> Option<String> {
        self.payload.as_ref()?.headers.iter().find_map(|h| {
            (h.name.eq_ignore_ascii_case(name) && !h.value.trim().is_empty())
                .then(|| h.value.trim().to_string())
        })
    }

    fn into_email(self, account: &str) -> Option<Email> {
        let to = self
            .to
            .clone()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| self.header("To"))
            .unwrap_or_default();
        let cc = self.header("Cc").unwrap_or_default();
        let message_id = self.message_id?;
        Some(Email {
            message_id,
            thread_id: self.thread_id,
            from: self.from.or(self.sender).unwrap_or_default(),
            to,
            cc,
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
        // No inline user_id: it is NOT a harmless duplicate of the envelope
        // user_id — the tool forwards it as the Gmail API `users/<id>/…` path
        // segment, and a Composio entity id there 403s with "Delegation
        // denied", killing thread context entirely. Verified live 2026-07-08.
        let args = serde_json::json!({
            "thread_id": thread_id,
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
        // Full logic (incl. the multi-recipient split for `recipient_email` /
        // `extra_recipients`, #439) lives in the inherent attachment-aware
        // variant (#417).
        self.create_draft_with_attachment(entity_id, to, subject, body, thread_id, None, &[], &[])
            .await
    }

    async fn create_draft_with_cc(
        &self,
        entity_id: &str,
        to: &str,
        cc: &[String],
        subject: &str,
        body: &str,
        thread_id: Option<&str>,
    ) -> Result<String, GmailError> {
        self.create_draft_with_attachment(entity_id, to, subject, body, thread_id, None, cc, &[])
            .await
    }

    async fn send_draft(
        &self,
        entity_id: &str,
        draft_id: &str,
    ) -> Result<Option<String>, GmailError> {
        let args = serde_json::json!({ "draft_id": draft_id });
        let v = self.execute("GMAIL_SEND_DRAFT", entity_id, args).await?;
        // Gmail's drafts.send returns the resulting Message resource, so `id`
        // here is the SENT message's id (not the draft's — that one is
        // consumed by the send). Key order matters: prefer the explicit
        // message-id spellings before falling back to a bare `id`.
        const SENT_ID_KEYS: &[&str] = &["message_id", "messageId", "id"];
        let sent_id = find_string_field(&v, SENT_ID_KEYS);
        if sent_id.is_none() {
            // Not fatal — the mail went out. But self-send recognition now
            // leans on the thread+time fallback, so make the gap visible.
            tracing::warn!(
                draft_id,
                "GMAIL_SEND_DRAFT returned no message id; \
                 falling back to thread-proximity self-send detection (#449)"
            );
        }
        Ok(sent_id)
    }

    async fn delete_draft(&self, entity_id: &str, draft_id: &str) -> Result<(), GmailError> {
        let args = serde_json::json!({ "draft_id": draft_id });
        self.execute("GMAIL_DELETE_DRAFT", entity_id, args).await?;
        Ok(())
    }
}
