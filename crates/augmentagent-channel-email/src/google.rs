//! Direct Gmail client on the operator's own Google OAuth client (#1573).
//!
//! Composio was only ever a proxy in front of the same Gmail REST endpoints:
//! it hosted a Google OAuth client, kept each mailbox's refresh token, and
//! metered every call. This module does the first two itself and talks to
//! `gmail.googleapis.com` directly, so a mailbox connected this way costs
//! zero Composio calls for reads, drafts, sends and attachments.
//!
//! Routing: a mailbox connected through Google is registered in
//! `gmail_accounts` with the entity id `google:<address>` (see
//! [`entity_id_for`]). `ComposioClient` checks that prefix on every call and
//! hands Google-connected entities to [`GoogleGmail`]; Composio-connected
//! entities keep their existing path, so both kinds coexist until the last
//! Composio mailbox is reconnected.
//!
//! Credentials: the OAuth *client* (id + secret) comes from
//! `GOOGLE_OAUTH_CLIENT_ID` / `GOOGLE_OAUTH_CLIENT_SECRET`. Each mailbox's
//! refresh token lives in the process credential store under
//! `augmentagent/google/<address>` — the same store every other channel uses,
//! owner-only files on Linux. Set the Google Cloud consent screen to
//! *Production*: an app left in *Testing* expires refresh tokens after seven
//! days, which would silently disconnect every mailbox weekly.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};

use augmentagent_auth::{default_store, CredentialStore};
use augmentagent_store::Email;

use crate::gmail::{split_recipient_entries, AttachmentMeta, GmailError};

/// Entity-id prefix that marks a Google-connected mailbox. Everything after
/// it is the mailbox address, lower-cased.
pub const ENTITY_PREFIX: &str = "google:";

/// Credential-store platform for per-mailbox Google refresh tokens.
pub const AUTH_PLATFORM: &str = "google";

/// `.env` keys for the operator's OAuth client.
pub const CLIENT_ID_ENV: &str = "GOOGLE_OAUTH_CLIENT_ID";
pub const CLIENT_SECRET_ENV: &str = "GOOGLE_OAUTH_CLIENT_SECRET";

/// Scopes requested at connect time. One consent covers every Google
/// surface the daemon reads, so the Calendar/Drive/People ports reuse the
/// token this module stores instead of asking the owner to consent again.
pub const SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/gmail.modify",
    "https://www.googleapis.com/auth/calendar",
    "https://www.googleapis.com/auth/drive",
    "https://www.googleapis.com/auth/contacts.readonly",
];

const API_BASE: &str = "https://gmail.googleapis.com/gmail/v1";
const TOKEN_BASE: &str = "https://oauth2.googleapis.com";
const AUTH_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";

/// Access tokens last an hour; refresh this early so one cannot expire
/// between the check and the request.
const TOKEN_SKEW: Duration = Duration::from_secs(60);
/// `messages.get` fan-out per search page.
const GET_CONCURRENCY: usize = 6;
/// Gmail's `messages.list` page ceiling.
const LIST_PAGE_MAX: u32 = 100;
const MAX_ATTEMPTS: u32 = 3;

/// `google:<address>` for a mailbox address.
pub fn entity_id_for(email: &str) -> String {
    format!("{ENTITY_PREFIX}{}", email.trim().to_ascii_lowercase())
}

/// The mailbox address behind a Google-connected entity id; `None` for a
/// Composio entity.
pub fn email_of(entity_id: &str) -> Option<&str> {
    entity_id
        .strip_prefix(ENTITY_PREFIX)
        .filter(|rest| !rest.is_empty())
}

/// The operator's OAuth client. Not a secret store: the id is public by
/// design and the secret only ever travels to Google's token endpoint.
#[derive(Clone)]
pub struct OAuthApp {
    pub client_id: String,
    client_secret: String,
}

impl std::fmt::Debug for OAuthApp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthApp")
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .finish()
    }
}

impl OAuthApp {
    pub fn new(client_id: impl Into<String>, client_secret: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
            client_secret: client_secret.into(),
        }
    }

    /// Read the client from the environment. The error names the missing
    /// variable so a half-configured `.env` is diagnosable from the log.
    pub fn from_env() -> Result<Self, GmailError> {
        let read = |key: &str| {
            std::env::var(key)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .ok_or_else(|| {
                    GmailError::Invalid(format!(
                        "{key} is not set; the direct Google client needs the operator's OAuth client (see .env.example)"
                    ))
                })
        };
        Ok(Self::new(read(CLIENT_ID_ENV)?, read(CLIENT_SECRET_ENV)?))
    }

    /// True when both variables are present — the dashboard uses this to
    /// decide whether "Add Gmail" goes through Google or through Composio.
    pub fn configured() -> bool {
        Self::from_env().is_ok()
    }

    /// The consent URL for one mailbox connect. `access_type=offline` plus
    /// `prompt=consent` is what makes Google return a refresh token every
    /// time, not only on the first consent for this client.
    ///
    /// `login_hint` (a mailbox address) makes Google preselect that account
    /// in its chooser, so a caller connecting several mailboxes in a row
    /// lands each consent on the intended one.
    pub fn auth_url(&self, redirect_uri: &str, state: &str, login_hint: Option<&str>) -> String {
        let scope = SCOPES.join(" ");
        let mut params = vec![
            ("client_id", self.client_id.as_str()),
            ("redirect_uri", redirect_uri),
            ("response_type", "code"),
            ("scope", scope.as_str()),
            ("access_type", "offline"),
            ("prompt", "consent"),
            ("include_granted_scopes", "true"),
            ("state", state),
        ];
        if let Some(hint) = login_hint.map(str::trim).filter(|h| !h.is_empty()) {
            params.push(("login_hint", hint));
        }
        let url = reqwest::Url::parse_with_params(AUTH_ENDPOINT, &params)
            .expect("static auth endpoint parses");
        url.to_string()
    }
}

/// One mailbox's stored grant, serialized as JSON into the credential store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredToken {
    pub email: String,
    pub refresh_token: String,
    #[serde(default)]
    pub access_token: Option<String>,
    /// Wall-clock expiry of `access_token`, epoch ms.
    #[serde(default)]
    pub expires_at_ms: Option<i64>,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub connected_at_ms: i64,
    /// Set when Google answered `invalid_grant`: the owner revoked access or
    /// the app's Testing-status token expired. The dashboard shows
    /// "Reconnect" for these instead of a live mailbox.
    #[serde(default)]
    pub revoked_at_ms: Option<i64>,
    #[serde(default)]
    pub revoke_reason: Option<String>,
}

impl StoredToken {
    pub fn is_revoked(&self) -> bool {
        self.revoked_at_ms.is_some()
    }

    fn account_key(email: &str) -> String {
        email.trim().to_ascii_lowercase()
    }
}

/// Attachment bytes for a direct-client draft. The Composio path uploads to
/// Composio's object store first; here the file rides inside the MIME body.
#[derive(Debug, Clone)]
pub struct LocalAttachment {
    pub name: String,
    pub mimetype: String,
    pub bytes: Vec<u8>,
}

impl LocalAttachment {
    pub fn read(path: &std::path::Path) -> Result<Self, GmailError> {
        let bytes = std::fs::read(path)
            .map_err(|e| GmailError::Decode(format!("read attachment {}: {e}", path.display())))?;
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("attachment")
            .to_string();
        let mimetype = crate::gmail::guess_mimetype(&name).to_string();
        Ok(Self {
            name,
            mimetype,
            bytes,
        })
    }
}

/// What a draft is built from. Shared by create and update.
#[derive(Debug, Clone, Default)]
pub struct DraftSpec<'a> {
    pub to: &'a str,
    pub cc: &'a [String],
    pub bcc: &'a [String],
    pub subject: &'a str,
    pub body: &'a str,
    pub thread_id: Option<&'a str>,
    pub attachment: Option<&'a LocalAttachment>,
}

struct CachedToken {
    access_token: String,
    expires_at: Instant,
}

/// Gmail REST client for Google-connected mailboxes. One instance serves
/// every mailbox; calls are keyed by address.
pub struct GoogleGmail {
    http: reqwest::Client,
    api_base: String,
    token_base: String,
    app: OAuthApp,
    creds: Arc<dyn CredentialStore>,
    cache: Mutex<HashMap<String, CachedToken>>,
}

impl std::fmt::Debug for GoogleGmail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoogleGmail")
            .field("api_base", &self.api_base)
            .field("app", &self.app)
            .finish()
    }
}

impl GoogleGmail {
    pub fn new(app: OAuthApp) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(120))
                .build()
                .expect("reqwest client"),
            api_base: API_BASE.to_string(),
            token_base: TOKEN_BASE.to_string(),
            app,
            creds: default_store(),
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn from_env() -> Result<Self, GmailError> {
        Ok(Self::new(OAuthApp::from_env()?))
    }

    /// Test hook: point both the API and the token endpoint at a fake.
    pub fn with_base_urls(mut self, api_base: String, token_base: String) -> Self {
        self.api_base = api_base;
        self.token_base = token_base;
        self
    }

    /// Test hook / embedding: keep tokens somewhere other than the process
    /// default credential store.
    pub fn with_credential_store(mut self, creds: Arc<dyn CredentialStore>) -> Self {
        self.creds = creds;
        self
    }

    pub fn app(&self) -> &OAuthApp {
        &self.app
    }

    // ----- tokens ---------------------------------------------------------

    /// The stored grant for a mailbox, or `Invalid` naming the address when
    /// nothing is stored.
    pub fn token(&self, email: &str) -> Result<StoredToken, GmailError> {
        let key = StoredToken::account_key(email);
        let bytes = self.creds.get(AUTH_PLATFORM, &key).map_err(|e| {
            GmailError::Invalid(format!(
                "no Google credential stored for {email} ({e}); connect it from the dashboard"
            ))
        })?;
        serde_json::from_slice(&bytes)
            .map_err(|e| GmailError::Decode(format!("stored Google credential for {email}: {e}")))
    }

    /// True when a grant is stored for the address (revoked or not).
    pub fn has_token(&self, email: &str) -> bool {
        self.creds
            .exists(AUTH_PLATFORM, &StoredToken::account_key(email))
    }

    fn save_token(&self, token: &StoredToken) -> Result<(), GmailError> {
        let payload = serde_json::to_vec(token)
            .map_err(|e| GmailError::Decode(format!("serialize Google credential: {e}")))?;
        self.creds
            .put(AUTH_PLATFORM, &StoredToken::account_key(&token.email), &payload)
            .map_err(|e| GmailError::Invalid(format!("store Google credential: {e}")))
    }

    fn delete_token(&self, email: &str) -> Result<(), GmailError> {
        self.cache
            .lock()
            .expect("token cache poisoned")
            .remove(&StoredToken::account_key(email));
        self.creds
            .delete(AUTH_PLATFORM, &StoredToken::account_key(email))
            .map_err(|e| GmailError::Invalid(format!("delete Google credential: {e}")))
    }

    /// Finish an OAuth consent: exchange the authorization code, learn which
    /// mailbox it belongs to from `users.getProfile`, and store the grant.
    /// Returns the stored token (address included) so the caller can
    /// register the account.
    pub async fn connect(&self, code: &str, redirect_uri: &str) -> Result<StoredToken, GmailError> {
        let url = format!("{}/token", self.token_base);
        let resp = self
            .http
            .post(&url)
            .form(&[
                ("client_id", self.app.client_id.as_str()),
                ("client_secret", self.app.client_secret.as_str()),
                ("code", code),
                ("redirect_uri", redirect_uri),
                ("grant_type", "authorization_code"),
            ])
            .send()
            .await?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(GmailError::Google {
                message: format!("authorization code exchange → {status}: {}", oauth_error_summary(&body)),
            });
        }
        let parsed: TokenResponse = serde_json::from_str(&body)
            .map_err(|e| GmailError::Decode(format!("google token response: {e}")))?;
        let email = self
            .profile_with_token(&parsed.access_token)
            .await?
            .email_address
            .filter(|e| !e.is_empty())
            .ok_or_else(|| GmailError::Decode("users.getProfile returned no emailAddress".into()))?
            .to_ascii_lowercase();

        // `prompt=consent` makes Google send a refresh token every time, but
        // keep the previous one as a fallback so a reconnect never ends up
        // with no offline grant at all.
        let refresh_token = match parsed.refresh_token.filter(|t| !t.is_empty()) {
            Some(t) => t,
            None => self
                .token(&email)
                .ok()
                .filter(|t| !t.is_revoked())
                .map(|t| t.refresh_token)
                .ok_or_else(|| {
                    GmailError::Google {
                        message: format!(
                            "Google returned no refresh token for {email}; remove the app at \
                             https://myaccount.google.com/permissions and connect again"
                        ),
                    }
                })?,
        };
        let now = wallclock_ms();
        let token = StoredToken {
            email: email.clone(),
            refresh_token,
            access_token: Some(parsed.access_token.clone()),
            expires_at_ms: Some(now + parsed.expires_in.unwrap_or(3600).max(1) * 1000),
            scopes: parsed
                .scope
                .unwrap_or_default()
                .split_whitespace()
                .map(str::to_string)
                .collect(),
            connected_at_ms: now,
            revoked_at_ms: None,
            revoke_reason: None,
        };
        self.save_token(&token)?;
        self.cache.lock().expect("token cache poisoned").insert(
            StoredToken::account_key(&email),
            CachedToken {
                access_token: parsed.access_token,
                expires_at: Instant::now()
                    + Duration::from_secs(parsed.expires_in.unwrap_or(3600).max(1) as u64)
                        .saturating_sub(TOKEN_SKEW),
            },
        );
        Ok(token)
    }

    /// Revoke the grant at Google and forget it locally. Returns whether
    /// Google acknowledged the revocation; the local delete happens either
    /// way, since a token Google already invalidated still must not linger.
    pub async fn disconnect(&self, email: &str) -> Result<bool, GmailError> {
        let revoked = match self.token(email) {
            Ok(token) => {
                let url = format!("{}/revoke", self.token_base);
                match self
                    .http
                    .post(&url)
                    .form(&[("token", token.refresh_token.as_str())])
                    .send()
                    .await
                {
                    Ok(resp) => resp.status().is_success(),
                    Err(e) => {
                        tracing::warn!(email, "google token revoke failed: {e}");
                        false
                    }
                }
            }
            Err(_) => false,
        };
        self.delete_token(email)?;
        Ok(revoked)
    }

    /// A valid access token for the mailbox, refreshed when within
    /// [`TOKEN_SKEW`] of expiry. Public so the Calendar/Drive/People clients
    /// can share the grant.
    pub async fn access_token(&self, email: &str) -> Result<String, GmailError> {
        let key = StoredToken::account_key(email);
        if let Some(cached) = self.cache.lock().expect("token cache poisoned").get(&key) {
            if cached.expires_at > Instant::now() {
                return Ok(cached.access_token.clone());
            }
        }
        // A sibling process (the CLI next to the daemon) may have refreshed
        // already; honour a persisted token that is still comfortably valid.
        let stored = self.token(email)?;
        if stored.is_revoked() {
            return Err(revoked_error(email, stored.revoke_reason.as_deref()));
        }
        if let (Some(access), Some(exp)) = (&stored.access_token, stored.expires_at_ms) {
            let remaining_ms = exp - wallclock_ms();
            if remaining_ms > TOKEN_SKEW.as_millis() as i64 {
                self.cache.lock().expect("token cache poisoned").insert(
                    key,
                    CachedToken {
                        access_token: access.clone(),
                        expires_at: Instant::now() + Duration::from_millis(remaining_ms as u64)
                            - TOKEN_SKEW,
                    },
                );
                return Ok(access.clone());
            }
        }
        self.refresh(stored).await
    }

    async fn refresh(&self, mut stored: StoredToken) -> Result<String, GmailError> {
        let email = stored.email.clone();
        let url = format!("{}/token", self.token_base);
        let resp = self
            .http
            .post(&url)
            .form(&[
                ("client_id", self.app.client_id.as_str()),
                ("client_secret", self.app.client_secret.as_str()),
                ("refresh_token", stored.refresh_token.as_str()),
                ("grant_type", "refresh_token"),
            ])
            .send()
            .await?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            let summary = oauth_error_summary(&body);
            if summary.contains("invalid_grant") {
                // The owner revoked access, or a Testing-status app expired
                // the token. Latch it so every later call fails fast with the
                // same instruction instead of re-asking Google each tick.
                stored.revoked_at_ms = Some(wallclock_ms());
                stored.revoke_reason = Some(summary.clone());
                stored.access_token = None;
                stored.expires_at_ms = None;
                if let Err(e) = self.save_token(&stored) {
                    tracing::warn!(email, "could not latch revoked Google credential: {e}");
                }
                return Err(revoked_error(&email, Some(&summary)));
            }
            return Err(GmailError::Google {
                message: format!("token refresh for {email} → {status}: {summary}"),
            });
        }
        let parsed: TokenResponse = serde_json::from_str(&body)
            .map_err(|e| GmailError::Decode(format!("google token response: {e}")))?;
        let ttl_secs = parsed.expires_in.unwrap_or(3600).max(1);
        stored.access_token = Some(parsed.access_token.clone());
        stored.expires_at_ms = Some(wallclock_ms() + ttl_secs * 1000);
        if let Some(rt) = parsed.refresh_token.filter(|t| !t.is_empty()) {
            stored.refresh_token = rt;
        }
        if let Err(e) = self.save_token(&stored) {
            // Non-fatal: the in-memory copy serves this process.
            tracing::warn!(email, "could not persist refreshed Google access token: {e}");
        }
        self.cache.lock().expect("token cache poisoned").insert(
            StoredToken::account_key(&email),
            CachedToken {
                access_token: parsed.access_token.clone(),
                expires_at: Instant::now()
                    + Duration::from_secs(ttl_secs as u64).saturating_sub(TOKEN_SKEW),
            },
        );
        Ok(parsed.access_token)
    }

    /// Drop the cached access token and mint a fresh one (used after a 401).
    async fn force_refresh(&self, email: &str) -> Result<String, GmailError> {
        self.cache
            .lock()
            .expect("token cache poisoned")
            .remove(&StoredToken::account_key(email));
        let mut stored = self.token(email)?;
        if stored.is_revoked() {
            return Err(revoked_error(email, stored.revoke_reason.as_deref()));
        }
        stored.access_token = None;
        stored.expires_at_ms = None;
        self.refresh(stored).await
    }

    // ----- transport ------------------------------------------------------

    /// One Gmail API call. `Ok(None)` is a 404 — the caller decides whether
    /// that is an error. A 401 refreshes the token once; 429 and 5xx back
    /// off and retry up to [`MAX_ATTEMPTS`] times.
    async fn call(
        &self,
        email: &str,
        method: reqwest::Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&serde_json::Value>,
    ) -> Result<Option<serde_json::Value>, GmailError> {
        let url = format!("{}/{path}", self.api_base);
        let mut token = self.access_token(email).await?;
        let mut refreshed = false;
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let mut req = self
                .http
                .request(method.clone(), &url)
                .query(query)
                .bearer_auth(&token);
            if let Some(b) = body {
                req = req.json(b);
            }
            let resp = match req.send().await {
                Ok(r) => r,
                Err(e) if attempt < MAX_ATTEMPTS && (e.is_timeout() || e.is_connect() || e.is_request()) => {
                    tracing::warn!(path, attempt, "gmail transport error; retrying: {e}");
                    backoff(attempt).await;
                    continue;
                }
                Err(e) => return Err(GmailError::Http(e)),
            };
            let status = resp.status();
            if status.is_success() {
                if status.as_u16() == 204 {
                    return Ok(Some(serde_json::Value::Null));
                }
                let text = resp.text().await.unwrap_or_default();
                if text.trim().is_empty() {
                    return Ok(Some(serde_json::Value::Null));
                }
                return serde_json::from_str(&text)
                    .map(Some)
                    .map_err(|e| GmailError::Decode(format!("gmail {path}: {e}")));
            }
            if status.as_u16() == 404 {
                return Ok(None);
            }
            if status.as_u16() == 401 && !refreshed {
                refreshed = true;
                token = self.force_refresh(email).await?;
                continue;
            }
            let text = resp.text().await.unwrap_or_default();
            let retryable = status.as_u16() == 429
                || status.is_server_error()
                || (status.as_u16() == 403 && text.contains("ateLimitExceeded"));
            if retryable && attempt < MAX_ATTEMPTS {
                tracing::warn!(path, status = %status, attempt, "gmail retryable failure; backing off");
                backoff(attempt).await;
                continue;
            }
            return Err(GmailError::Google {
                message: format!("{path} → {status}: {}", api_error_summary(&text)),
            });
        }
    }

    async fn get(
        &self,
        email: &str,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<Option<serde_json::Value>, GmailError> {
        self.call(email, reqwest::Method::GET, path, query, None).await
    }

    /// GET that treats 404 as an error naming the path.
    async fn get_required(
        &self,
        email: &str,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<serde_json::Value, GmailError> {
        self.get(email, path, query).await?.ok_or_else(|| GmailError::Google {
            message: format!("{path} → 404 Not Found"),
        })
    }

    async fn profile_with_token(&self, access_token: &str) -> Result<Profile, GmailError> {
        let url = format!("{}/users/me/profile", self.api_base);
        let resp = self.http.get(&url).bearer_auth(access_token).send().await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(GmailError::Google {
                message: format!("users.getProfile → {status}: {}", api_error_summary(&text)),
            });
        }
        serde_json::from_str(&text).map_err(|e| GmailError::Decode(format!("gmail profile: {e}")))
    }

    // ----- mailbox operations --------------------------------------------

    /// `users.getProfile` — the address Google says this grant belongs to.
    pub async fn profile_email(&self, email: &str) -> Result<String, GmailError> {
        let v = self.get_required(email, "users/me/profile", &[]).await?;
        v.get("emailAddress")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| GmailError::Decode("users.getProfile returned no emailAddress".into()))
    }

    /// `messages.list(q)` walked up to `max_total` ids, then `messages.get`
    /// (format=full) for each, newest first as Gmail lists them.
    pub async fn search(
        &self,
        email: &str,
        query: &str,
        max_total: u32,
    ) -> Result<Vec<Email>, GmailError> {
        if max_total == 0 {
            return Ok(Vec::new());
        }
        let mut ids: Vec<String> = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let want = max_total.saturating_sub(ids.len() as u32);
            if want == 0 {
                break;
            }
            let mut q = vec![
                ("q", query.to_string()),
                ("maxResults", want.min(LIST_PAGE_MAX).to_string()),
            ];
            if let Some(t) = &page_token {
                q.push(("pageToken", t.clone()));
            }
            let v = self.get_required(email, "users/me/messages", &q).await?;
            let page: MessagesList = serde_json::from_value(v)
                .map_err(|e| GmailError::Decode(format!("messages.list: {e}")))?;
            let before = ids.len();
            for m in page.messages {
                if !ids.contains(&m.id) {
                    ids.push(m.id);
                }
            }
            let next = page.next_page_token.filter(|t| !t.is_empty());
            if next.is_none() || next == page_token || ids.len() == before {
                break;
            }
            page_token = next;
        }
        ids.truncate(max_total as usize);
        self.fetch_messages(email, &ids).await
    }

    async fn fetch_messages(&self, email: &str, ids: &[String]) -> Result<Vec<Email>, GmailError> {
        let fetched: Vec<Result<Option<Email>, GmailError>> = stream::iter(ids.iter().cloned())
            .map(|id| async move {
                let path = format!("users/me/messages/{id}");
                match self.get(email, &path, &[("format", "full".to_string())]).await? {
                    // Deleted between list and get.
                    None => Ok(None),
                    Some(v) => {
                        let msg: GmailMessage = serde_json::from_value(v)
                            .map_err(|e| GmailError::Decode(format!("messages.get {id}: {e}")))?;
                        Ok(Some(msg.into_email(&entity_id_for(email))))
                    }
                }
            })
            .buffered(GET_CONCURRENCY)
            .collect()
            .await;
        let mut out = Vec::with_capacity(ids.len());
        for r in fetched {
            if let Some(e) = r? {
                out.push(e);
            }
        }
        Ok(out)
    }

    /// `threads.get(format=full)`: every message oldest first, trimmed to
    /// the last `max` when `max > 0`.
    pub async fn thread_messages(
        &self,
        email: &str,
        thread_id: &str,
        max: u32,
    ) -> Result<Vec<Email>, GmailError> {
        let path = format!("users/me/threads/{thread_id}");
        let v = self
            .get_required(email, &path, &[("format", "full".to_string())])
            .await?;
        let thread: GmailThread = serde_json::from_value(v)
            .map_err(|e| GmailError::Decode(format!("threads.get {thread_id}: {e}")))?;
        let entity = entity_id_for(email);
        let mut msgs: Vec<Email> = thread
            .messages
            .into_iter()
            .map(|m| m.into_email(&entity))
            .collect();
        msgs.sort_by_key(|m| crate::outbound::parse_rfc2822_or_ms(&m.date));
        if max > 0 && msgs.len() > max as usize {
            let drop = msgs.len() - max as usize;
            msgs.drain(0..drop);
        }
        Ok(msgs)
    }

    /// Message id or thread id → the thread id it belongs to, scoped to this
    /// mailbox. Mirrors `ComposioClient::resolve_thread_id`.
    pub async fn resolve_thread_id(&self, email: &str, id: &str) -> Result<String, GmailError> {
        let path = format!("users/me/messages/{id}");
        if let Some(v) = self
            .get(email, &path, &[("format", "minimal".to_string())])
            .await?
        {
            if let Some(t) = v.get("threadId").and_then(serde_json::Value::as_str) {
                return Ok(t.to_string());
            }
        }
        let path = format!("users/me/threads/{id}");
        if self
            .get(email, &path, &[("format", "minimal".to_string())])
            .await?
            .is_some()
        {
            return Ok(id.to_string());
        }
        Err(GmailError::Google {
            message: format!("id {id} is neither a message nor a thread in {email}'s mailbox"),
        })
    }

    /// The thread an existing draft is attached to; `None` for a standalone
    /// draft (Gmail gives those their own thread, threadId == message id).
    pub async fn draft_thread_id(
        &self,
        email: &str,
        draft_id: &str,
    ) -> Result<Option<String>, GmailError> {
        let path = format!("users/me/drafts/{draft_id}");
        let v = self
            .get_required(email, &path, &[("format", "minimal".to_string())])
            .await?;
        let msg_id = v.pointer("/message/id").and_then(serde_json::Value::as_str);
        let thread_id = v
            .pointer("/message/threadId")
            .and_then(serde_json::Value::as_str);
        Ok(match (msg_id, thread_id) {
            (Some(m), Some(t)) if m != t => Some(t.to_string()),
            _ => None,
        })
    }

    /// `drafts.create`. Returns the draft id.
    pub async fn create_draft(&self, email: &str, spec: DraftSpec<'_>) -> Result<String, GmailError> {
        let body = self.draft_body(email, &spec).await?;
        let v = self
            .call(email, reqwest::Method::POST, "users/me/drafts", &[], Some(&body))
            .await?
            .ok_or_else(|| GmailError::Google {
                message: "drafts.create → 404".into(),
            })?;
        draft_id_of(&v)
    }

    /// `drafts.update` — a real in-place update, so the id is unchanged.
    pub async fn update_draft(
        &self,
        email: &str,
        draft_id: &str,
        spec: DraftSpec<'_>,
    ) -> Result<String, GmailError> {
        let body = self.draft_body(email, &spec).await?;
        let path = format!("users/me/drafts/{draft_id}");
        let v = self
            .call(email, reqwest::Method::PUT, &path, &[], Some(&body))
            .await?
            .ok_or_else(|| GmailError::Google {
                message: format!("drafts.update {draft_id} → 404: the draft no longer exists"),
            })?;
        draft_id_of(&v)
    }

    /// `drafts.send`. Returns the SENT message id.
    pub async fn send_draft(&self, email: &str, draft_id: &str) -> Result<Option<String>, GmailError> {
        let body = serde_json::json!({ "id": draft_id });
        let v = self
            .call(email, reqwest::Method::POST, "users/me/drafts/send", &[], Some(&body))
            .await?
            .ok_or_else(|| GmailError::Google {
                message: format!("drafts.send {draft_id} → 404: the draft no longer exists"),
            })?;
        Ok(v.get("id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string))
    }

    /// `drafts.delete`. A draft that is already gone counts as deleted.
    pub async fn delete_draft(&self, email: &str, draft_id: &str) -> Result<(), GmailError> {
        let path = format!("users/me/drafts/{draft_id}");
        self.call(email, reqwest::Method::DELETE, &path, &[], None)
            .await?;
        Ok(())
    }

    /// Attachment parts of a message, with the ids `attachment_bytes` needs.
    pub async fn list_attachments(
        &self,
        email: &str,
        message_id: &str,
    ) -> Result<Vec<AttachmentMeta>, GmailError> {
        let path = format!("users/me/messages/{message_id}");
        let v = self
            .get_required(email, &path, &[("format", "full".to_string())])
            .await?;
        let msg: GmailMessage = serde_json::from_value(v)
            .map_err(|e| GmailError::Decode(format!("messages.get {message_id}: {e}")))?;
        Ok(msg
            .payload
            .as_ref()
            .map(attachment_metas)
            .unwrap_or_default())
    }

    /// `messages.attachments.get` → the decoded bytes.
    pub async fn attachment_bytes(
        &self,
        email: &str,
        message_id: &str,
        attachment_id: &str,
    ) -> Result<Vec<u8>, GmailError> {
        let path = format!("users/me/messages/{message_id}/attachments/{attachment_id}");
        let v = self.get_required(email, &path, &[]).await?;
        let data = v
            .get("data")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| GmailError::Decode("attachments.get returned no data".into()))?;
        decode_base64url(data)
            .ok_or_else(|| GmailError::Decode("attachments.get data is not base64url".into()))
    }

    async fn draft_body(&self, email: &str, spec: &DraftSpec<'_>) -> Result<serde_json::Value, GmailError> {
        if split_recipient_entries(spec.to)
            .iter()
            .all(|e| e.trim().is_empty())
        {
            return Err(GmailError::Invalid(format!(
                "no recipient address in {:?}",
                spec.to
            )));
        }
        let mut threading = Threading::default();
        if let Some(thread_id) = spec.thread_id {
            threading = self.threading_headers(email, thread_id).await?;
        }
        let mime = build_mime(spec, &threading);
        let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mime.as_bytes());
        let mut message = serde_json::json!({ "raw": raw });
        if let Some(t) = spec.thread_id {
            message["threadId"] = serde_json::Value::String(t.to_string());
        }
        Ok(serde_json::json!({ "message": message }))
    }

    /// `In-Reply-To` / `References` for a reply on `thread_id`, taken from
    /// the thread's newest message. Gmail threads by `threadId` on its own,
    /// but every other client the recipient might use threads by these.
    async fn threading_headers(&self, email: &str, thread_id: &str) -> Result<Threading, GmailError> {
        let path = format!("users/me/threads/{thread_id}");
        let q = [
            ("format", "metadata".to_string()),
            ("metadataHeaders", "Message-ID".to_string()),
            ("metadataHeaders", "References".to_string()),
        ];
        let v = self.get(email, &path, &q).await?.ok_or_else(|| GmailError::Google {
            message: format!("thread {thread_id} was not found in {email}'s mailbox"),
        })?;
        let thread: GmailThread = serde_json::from_value(v)
            .map_err(|e| GmailError::Decode(format!("threads.get {thread_id}: {e}")))?;
        let newest = thread
            .messages
            .iter()
            .max_by_key(|m| m.internal_date.as_deref().and_then(|d| d.parse::<i64>().ok()).unwrap_or(0));
        Ok(match newest {
            Some(m) => {
                let message_id = m.header("Message-ID");
                let prior = m.header("References");
                let references = match (prior.is_empty(), message_id.is_empty()) {
                    (true, true) => String::new(),
                    (true, false) => message_id.clone(),
                    (false, true) => prior,
                    (false, false) => format!("{prior} {message_id}"),
                };
                Threading {
                    in_reply_to: message_id,
                    references,
                }
            }
            None => Threading::default(),
        })
    }
}

fn revoked_error(email: &str, reason: Option<&str>) -> GmailError {
    GmailError::Invalid(format!(
        "Google access for {email} was revoked or expired ({}); reconnect it from the dashboard",
        reason.unwrap_or("invalid_grant")
    ))
}

fn draft_id_of(v: &serde_json::Value) -> Result<String, GmailError> {
    v.get("id")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            GmailError::Decode(format!(
                "missing draft id in response: {}",
                serde_json::to_string(v).unwrap_or_default()
            ))
        })
}

/// `error` + `error_description` from an OAuth error body, or the raw text.
fn oauth_error_summary(body: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(v) => {
            let code = v.get("error").and_then(serde_json::Value::as_str).unwrap_or("");
            let desc = v
                .get("error_description")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            match (code.is_empty(), desc.is_empty()) {
                (false, false) => format!("{code}: {desc}"),
                (false, true) => code.to_string(),
                _ => body.trim().to_string(),
            }
        }
        Err(_) => body.trim().to_string(),
    }
}

/// `error.message` from a Gmail API error body, or the raw text.
fn api_error_summary(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| body.trim().to_string())
}

async fn backoff(attempt: u32) {
    let mult: u64 = 1 << attempt.min(5);
    tokio::time::sleep(Duration::from_millis(300 * mult)).await;
}

fn wallclock_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ----- MIME ------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
struct Threading {
    in_reply_to: String,
    references: String,
}

/// An RFC 5322 message for `drafts.create`'s `raw`. Plain-text body, base64
/// transfer encoding (safe for any line length or charset), multipart/mixed
/// when an attachment rides along.
fn build_mime(spec: &DraftSpec<'_>, threading: &Threading) -> String {
    let mut out = String::new();
    push_address_header(&mut out, "To", spec.to);
    if !spec.cc.is_empty() {
        push_address_header(&mut out, "Cc", &spec.cc.join(", "));
    }
    if !spec.bcc.is_empty() {
        push_address_header(&mut out, "Bcc", &spec.bcc.join(", "));
    }
    out.push_str("Subject: ");
    out.push_str(&encode_header_value(spec.subject));
    out.push_str("\r\n");
    if !threading.in_reply_to.is_empty() {
        out.push_str("In-Reply-To: ");
        out.push_str(&threading.in_reply_to);
        out.push_str("\r\n");
    }
    if !threading.references.is_empty() {
        out.push_str("References: ");
        out.push_str(&threading.references);
        out.push_str("\r\n");
    }
    out.push_str("MIME-Version: 1.0\r\n");

    let text_part = |out: &mut String| {
        out.push_str("Content-Type: text/plain; charset=\"UTF-8\"\r\n");
        out.push_str("Content-Transfer-Encoding: base64\r\n\r\n");
        out.push_str(&wrap_base64(spec.body.as_bytes()));
        out.push_str("\r\n");
    };

    match spec.attachment {
        None => text_part(&mut out),
        Some(att) => {
            let boundary = format!("=_augmentagent_{:x}", wallclock_ms());
            out.push_str(&format!(
                "Content-Type: multipart/mixed; boundary=\"{boundary}\"\r\n\r\n"
            ));
            out.push_str(&format!("--{boundary}\r\n"));
            text_part(&mut out);
            out.push_str(&format!("--{boundary}\r\n"));
            let name = att.name.replace('"', "");
            out.push_str(&format!(
                "Content-Type: {}; name=\"{name}\"\r\n",
                att.mimetype
            ));
            out.push_str(&format!(
                "Content-Disposition: attachment; filename=\"{name}\"\r\n"
            ));
            out.push_str("Content-Transfer-Encoding: base64\r\n\r\n");
            out.push_str(&wrap_base64(&att.bytes));
            out.push_str(&format!("\r\n--{boundary}--\r\n"));
        }
    }
    out
}

/// `Name: a@x, b@y` folded after each comma so a long list never exceeds
/// the 998-octet line limit.
fn push_address_header(out: &mut String, name: &str, raw: &str) {
    let entries: Vec<String> = split_recipient_entries(raw)
        .into_iter()
        .map(|e| e.trim().to_string())
        .filter(|e| !e.is_empty())
        .collect();
    if entries.is_empty() {
        return;
    }
    out.push_str(name);
    out.push_str(": ");
    out.push_str(&entries.join(",\r\n "));
    out.push_str("\r\n");
}

/// RFC 2047 encoded-words for a non-ASCII header value; ASCII passes
/// through untouched (with CR/LF stripped so it cannot inject headers).
fn encode_header_value(value: &str) -> String {
    let clean: String = value.chars().filter(|c| *c != '\r' && *c != '\n').collect();
    if clean.is_ascii() {
        return clean;
    }
    // Each encoded-word may be at most 75 chars: "=?UTF-8?B?" (10) + "?=" (2)
    // leaves 63 base64 chars ≈ 45 input bytes; split on char boundaries.
    let mut words = Vec::new();
    let mut chunk = String::new();
    for ch in clean.chars() {
        if chunk.len() + ch.len_utf8() > 45 {
            words.push(encoded_word(&chunk));
            chunk.clear();
        }
        chunk.push(ch);
    }
    if !chunk.is_empty() {
        words.push(encoded_word(&chunk));
    }
    words.join("\r\n ")
}

fn encoded_word(chunk: &str) -> String {
    format!(
        "=?UTF-8?B?{}?=",
        base64::engine::general_purpose::STANDARD.encode(chunk.as_bytes())
    )
}

fn wrap_base64(bytes: &[u8]) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    encoded
        .as_bytes()
        .chunks(76)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\r\n")
}

/// Gmail emits base64url, padded or not; accept both plus standard base64.
pub(crate) fn decode_base64url(input: &str) -> Option<Vec<u8>> {
    let cleaned: String = input
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '=')
        .map(|c| match c {
            '+' => '-',
            '/' => '_',
            other => other,
        })
        .collect();
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(cleaned.as_bytes())
        .ok()
}

// ----- wire shapes -----------------------------------------------------------

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Profile {
    #[serde(default)]
    email_address: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MessagesList {
    #[serde(default)]
    messages: Vec<MessageRef>,
    #[serde(default)]
    next_page_token: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct MessageRef {
    id: String,
}

#[derive(Debug, Default, Deserialize)]
struct GmailThread {
    #[serde(default)]
    messages: Vec<GmailMessage>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GmailMessage {
    #[serde(default)]
    id: String,
    #[serde(default)]
    thread_id: Option<String>,
    #[serde(default)]
    internal_date: Option<String>,
    #[serde(default)]
    payload: Option<MessagePart>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MessagePart {
    #[serde(default)]
    mime_type: Option<String>,
    #[serde(default)]
    filename: Option<String>,
    #[serde(default)]
    headers: Vec<PartHeader>,
    #[serde(default)]
    body: Option<PartBody>,
    #[serde(default)]
    parts: Vec<MessagePart>,
}

#[derive(Debug, Default, Deserialize)]
struct PartHeader {
    #[serde(default)]
    name: String,
    #[serde(default)]
    value: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PartBody {
    #[serde(default)]
    data: Option<String>,
    #[serde(default)]
    attachment_id: Option<String>,
}

impl GmailMessage {
    fn header(&self, name: &str) -> String {
        self.payload
            .as_ref()
            .and_then(|p| {
                p.headers.iter().find_map(|h| {
                    (h.name.eq_ignore_ascii_case(name) && !h.value.trim().is_empty())
                        .then(|| h.value.trim().to_string())
                })
            })
            .unwrap_or_default()
    }

    pub(crate) fn into_email(self, account_entity_id: &str) -> Email {
        let from = self.header("From");
        let to = self.header("To");
        let cc = self.header("Cc");
        let subject = self.header("Subject");
        let (body, attachments) = match &self.payload {
            Some(p) => (
                extract_body(p).unwrap_or_default(),
                attachment_metas(p).iter().map(AttachmentMeta::label).collect(),
            ),
            None => (String::new(), Vec::new()),
        };
        Email {
            message_id: self.id,
            thread_id: self.thread_id,
            from,
            to,
            cc,
            attachments,
            subject,
            body,
            // RFC 3339, the shape Composio's `messageTimestamp` had, so
            // listings print a readable date; `parse_rfc2822_or_ms` reads it
            // back to the same instant.
            date: self
                .internal_date
                .as_deref()
                .and_then(|d| d.parse::<i64>().ok())
                .and_then(chrono::DateTime::from_timestamp_millis)
                .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
                .or(self.internal_date)
                .unwrap_or_default(),
            account_entity_id: Some(account_entity_id.to_string()),
            platform: "gmail".into(),
            kind: "dm".into(),
        }
    }
}

/// Depth-first walk preferring `text/plain`, falling back to `text/html`.
fn extract_body(part: &MessagePart) -> Option<String> {
    fn walk(part: &MessagePart, want: &str) -> Option<String> {
        let mime = part.mime_type.as_deref().unwrap_or("");
        let is_attachment = part
            .filename
            .as_deref()
            .is_some_and(|f| !f.trim().is_empty());
        if mime.eq_ignore_ascii_case(want) && !is_attachment {
            if let Some(data) = part.body.as_ref().and_then(|b| b.data.as_deref()) {
                return decode_base64url(data).map(|b| String::from_utf8_lossy(&b).into_owned());
            }
        }
        part.parts.iter().find_map(|p| walk(p, want))
    }
    walk(part, "text/plain").or_else(|| walk(part, "text/html"))
}

/// Every part that carries a filename, with its attachment id when Gmail
/// stored the bytes out of line.
fn attachment_metas(part: &MessagePart) -> Vec<AttachmentMeta> {
    fn walk(part: &MessagePart, out: &mut Vec<AttachmentMeta>) {
        let name = part.filename.as_deref().map(str::trim).unwrap_or("");
        if !name.is_empty() {
            out.push(AttachmentMeta {
                filename: Some(name.to_string()),
                mime_type: part.mime_type.clone(),
                attachment_id: part
                    .body
                    .as_ref()
                    .and_then(|b| b.attachment_id.clone())
                    .filter(|id| !id.is_empty()),
            });
        }
        for p in &part.parts {
            walk(p, out);
        }
    }
    let mut out = Vec::new();
    walk(part, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use augmentagent_auth::MemoryCredentialStore;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[derive(Clone, Debug)]
    struct Seen {
        line: String,
        auth: String,
        body: String,
    }

    /// Scripted Google: each route is `(method, path fragment, status, body)`;
    /// the first match in order answers. A route whose fragment starts with
    /// `once:` is consumed after it answers, so a 401-then-200 sequence can
    /// be scripted.
    struct FakeGoogle {
        addr: SocketAddr,
        seen: Arc<Mutex<Vec<Seen>>>,
        hits: Arc<AtomicUsize>,
    }

    impl FakeGoogle {
        async fn start(routes: Vec<(&'static str, &'static str, u16, String)>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let seen = Arc::new(Mutex::new(Vec::new()));
            let hits = Arc::new(AtomicUsize::new(0));
            let routes = Arc::new(Mutex::new(routes));
            let (seen_c, hits_c) = (Arc::clone(&seen), Arc::clone(&hits));
            tokio::spawn(async move {
                loop {
                    let Ok((mut socket, _)) = listener.accept().await else {
                        break;
                    };
                    let routes = Arc::clone(&routes);
                    let seen = Arc::clone(&seen_c);
                    let hits = Arc::clone(&hits_c);
                    tokio::spawn(async move {
                        let mut buf = Vec::new();
                        let mut chunk = [0u8; 4096];
                        // Read headers, then the Content-Length body.
                        let (head_end, content_len) = loop {
                            let n = socket.read(&mut chunk).await.unwrap_or(0);
                            if n == 0 {
                                break (buf.len(), 0);
                            }
                            buf.extend_from_slice(&chunk[..n]);
                            if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
                                let head = String::from_utf8_lossy(&buf[..pos]).to_string();
                                let len = head
                                    .lines()
                                    .find_map(|l| {
                                        l.to_ascii_lowercase()
                                            .strip_prefix("content-length:")
                                            .and_then(|v| v.trim().parse::<usize>().ok())
                                    })
                                    .unwrap_or(0);
                                break (pos + 4, len);
                            }
                        };
                        while buf.len() < head_end + content_len {
                            let n = socket.read(&mut chunk).await.unwrap_or(0);
                            if n == 0 {
                                break;
                            }
                            buf.extend_from_slice(&chunk[..n]);
                        }
                        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                        let line = head.lines().next().unwrap_or_default().to_string();
                        let auth = head
                            .lines()
                            .find_map(|l| l.strip_prefix("authorization: ").or_else(|| l.strip_prefix("Authorization: ")))
                            .unwrap_or("")
                            .to_string();
                        let body = String::from_utf8_lossy(&buf[head_end..]).to_string();
                        hits.fetch_add(1, Ordering::SeqCst);
                        seen.lock().unwrap().push(Seen {
                            line: line.clone(),
                            auth,
                            body,
                        });
                        let (status, resp_body) = {
                            let mut routes = routes.lock().unwrap();
                            let idx = routes.iter().position(|(m, frag, _, _)| {
                                let frag = frag.strip_prefix("once:").unwrap_or(frag);
                                line.starts_with(&format!("{m} ")) && line.contains(frag)
                            });
                            match idx {
                                Some(i) => {
                                    let (_, frag, s, b) = routes[i].clone();
                                    if frag.starts_with("once:") {
                                        routes.remove(i);
                                    }
                                    (s, b)
                                }
                                None => (500, format!("{{\"unmatched\":\"{line}\"}}")),
                            }
                        };
                        let resp = format!(
                            "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{resp_body}",
                            resp_body.len()
                        );
                        let _ = socket.write_all(resp.as_bytes()).await;
                        let _ = socket.shutdown().await;
                    });
                }
            });
            Self { addr, seen, hits }
        }

        fn seen(&self) -> Vec<Seen> {
            self.seen.lock().unwrap().clone()
        }

        fn count(&self) -> usize {
            self.hits.load(Ordering::SeqCst)
        }
    }

    fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
        hay.windows(needle.len()).position(|w| w == needle)
    }

    const MAILBOX: &str = "owner@example.com";

    fn token_route(access: &str) -> (&'static str, &'static str, u16, String) {
        (
            "POST",
            "/token",
            200,
            format!(r#"{{"access_token":"{access}","expires_in":3600,"scope":"a b"}}"#),
        )
    }

    fn client_with_token(fake: &FakeGoogle, token: Option<StoredToken>) -> GoogleGmail {
        let creds: Arc<dyn CredentialStore> = Arc::new(MemoryCredentialStore::default());
        let g = GoogleGmail::new(OAuthApp::new("cid", "csecret"))
            .with_base_urls(format!("http://{}", fake.addr), format!("http://{}", fake.addr))
            .with_credential_store(creds);
        if let Some(t) = token {
            g.save_token(&t).unwrap();
        }
        g
    }

    fn stored(email: &str) -> StoredToken {
        StoredToken {
            email: email.into(),
            refresh_token: "rt-1".into(),
            access_token: None,
            expires_at_ms: None,
            scopes: vec![],
            connected_at_ms: 0,
            revoked_at_ms: None,
            revoke_reason: None,
        }
    }

    fn b64url(s: &str) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s.as_bytes())
    }

    fn decode_raw(body: &str) -> String {
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        let raw = v.pointer("/message/raw").and_then(|r| r.as_str()).unwrap();
        String::from_utf8(decode_base64url(raw).unwrap()).unwrap()
    }

    #[test]
    fn entity_id_helpers_round_trip_and_reject_composio_ids() {
        assert_eq!(entity_id_for(" Owner@Example.com "), "google:owner@example.com");
        assert_eq!(email_of("google:owner@example.com"), Some("owner@example.com"));
        assert_eq!(email_of("augmentagent-1787244023284"), None);
        assert_eq!(email_of("google:"), None);
    }

    #[test]
    fn auth_url_requests_offline_access_with_forced_consent_and_every_scope() {
        let app = OAuthApp::new("cid.apps.googleusercontent.com", "s");
        let url = app.auth_url("http://localhost:3000/oauth/google/callback", "st4te", None);
        let parsed = reqwest::Url::parse(&url).unwrap();
        let q: HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(parsed.host_str(), Some("accounts.google.com"));
        assert_eq!(q["client_id"], "cid.apps.googleusercontent.com");
        assert_eq!(q["redirect_uri"], "http://localhost:3000/oauth/google/callback");
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["access_type"], "offline");
        assert_eq!(q["prompt"], "consent");
        assert_eq!(q["state"], "st4te");
        for scope in SCOPES {
            assert!(q["scope"].split(' ').any(|s| s == *scope), "{scope} missing");
        }
        assert!(!url.contains("csecret") && !url.contains("client_secret"));
        assert!(!q.contains_key("login_hint"));
        let hinted = app.auth_url("http://localhost:3000/oauth/google/callback", "s", Some("me@example.com"));
        let hq: HashMap<_, _> = reqwest::Url::parse(&hinted).unwrap().query_pairs().into_owned().collect();
        assert_eq!(hq["login_hint"], "me@example.com");
    }

    #[test]
    fn mime_reply_carries_recipients_threading_headers_and_a_base64_body() {
        let cc = vec!["Cee <cee@example.com>".to_string()];
        let spec = DraftSpec {
            to: "\"Doe, John\" <j@example.com>, bo@example.com",
            cc: &cc,
            bcc: &[],
            subject: "Re: Hello",
            body: "line one\r\nline two — with a dash",
            thread_id: Some("t1"),
            attachment: None,
        };
        let threading = Threading {
            in_reply_to: "<abc@mail.example.com>".into(),
            references: "<root@mail.example.com> <abc@mail.example.com>".into(),
        };
        let mime = build_mime(&spec, &threading);
        assert!(mime.starts_with("To: \"Doe, John\" <j@example.com>,\r\n bo@example.com\r\n"), "{mime}");
        assert!(mime.contains("Cc: Cee <cee@example.com>\r\n"));
        assert!(!mime.contains("Bcc:"));
        assert!(mime.contains("Subject: Re: Hello\r\n"));
        assert!(mime.contains("In-Reply-To: <abc@mail.example.com>\r\n"));
        assert!(mime.contains("References: <root@mail.example.com> <abc@mail.example.com>\r\n"));
        assert!(mime.contains("Content-Type: text/plain; charset=\"UTF-8\"\r\nContent-Transfer-Encoding: base64\r\n\r\n"));
        let encoded = mime.rsplit("\r\n\r\n").next().unwrap().trim();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded.replace("\r\n", ""))
            .unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), spec.body);
    }

    #[test]
    fn mime_with_attachment_is_multipart_mixed_with_the_file_inline() {
        let att = LocalAttachment {
            name: "invoice.pdf".into(),
            mimetype: "application/pdf".into(),
            bytes: b"%PDF-1.4 fake".to_vec(),
        };
        let spec = DraftSpec {
            to: "a@example.com",
            subject: "Invoice",
            body: "attached",
            attachment: Some(&att),
            ..Default::default()
        };
        let mime = build_mime(&spec, &Threading::default());
        assert!(mime.contains("Content-Type: multipart/mixed; boundary=\""));
        assert!(mime.contains("Content-Type: application/pdf; name=\"invoice.pdf\"\r\n"));
        assert!(mime.contains("Content-Disposition: attachment; filename=\"invoice.pdf\"\r\n"));
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"%PDF-1.4 fake");
        assert!(mime.contains(&b64));
        assert!(mime.trim_end().ends_with("--"));
    }

    #[test]
    fn header_values_are_encoded_only_when_non_ascii_and_never_carry_newlines() {
        assert_eq!(encode_header_value("Plain subject"), "Plain subject");
        assert_eq!(encode_header_value("inj\r\nected"), "injected");
        let enc = encode_header_value("Réunion — démo");
        assert!(enc.starts_with("=?UTF-8?B?") && enc.ends_with("?="), "{enc}");
        let long: String = "é".repeat(60);
        let encoded = encode_header_value(&long);
        let words: Vec<&str> = encoded.split("\r\n ").collect();
        assert!(words.len() >= 2);
        assert!(words.iter().all(|w| w.len() <= 75), "{words:?}");
    }

    #[test]
    fn message_body_prefers_text_plain_and_lists_attachments_like_composio() {
        let v = serde_json::json!({
            "id": "m1", "threadId": "t1", "internalDate": "1792850400000",
            "payload": {
                "mimeType": "multipart/mixed",
                "headers": [
                    {"name": "From", "value": "Ann <ann@example.com>"},
                    {"name": "To", "value": "owner@example.com"},
                    {"name": "CC", "value": "cee@example.com"},
                    {"name": "Subject", "value": "Resume"}
                ],
                "parts": [
                    {"mimeType": "multipart/alternative", "parts": [
                        {"mimeType": "text/html", "body": {"data": b64url("<b>hi</b>")}},
                        {"mimeType": "text/plain", "body": {"data": b64url("hi there")}}
                    ]},
                    {"mimeType": "application/pdf", "filename": "resume.pdf",
                     "body": {"attachmentId": "ANG-1", "size": 10}}
                ]
            }
        });
        let msg: GmailMessage = serde_json::from_value(v).unwrap();
        let metas = attachment_metas(msg.payload.as_ref().unwrap());
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].attachment_id.as_deref(), Some("ANG-1"));
        let e = msg.into_email("google:owner@example.com");
        assert_eq!(e.message_id, "m1");
        assert_eq!(e.thread_id.as_deref(), Some("t1"));
        assert_eq!(e.from, "Ann <ann@example.com>");
        assert_eq!(e.to, "owner@example.com");
        assert_eq!(e.cc, "cee@example.com");
        assert_eq!(e.subject, "Resume");
        assert_eq!(e.body, "hi there");
        assert_eq!(e.attachments, vec!["resume.pdf (application/pdf)"]);
        assert_eq!(crate::outbound::parse_rfc2822_or_ms(&e.date), 1_792_850_400_000);
        assert_eq!(e.account_entity_id.as_deref(), Some("google:owner@example.com"));

        // HTML-only mail still yields a body.
        let v = serde_json::json!({"id": "m2", "payload": {
            "mimeType": "text/html", "body": {"data": b64url("<p>only html</p>")}}});
        let e = serde_json::from_value::<GmailMessage>(v).unwrap().into_email("x");
        assert_eq!(e.body, "<p>only html</p>");
    }

    #[test]
    fn base64url_decoder_accepts_padded_unpadded_and_standard_alphabets() {
        assert_eq!(decode_base64url("aGk").unwrap(), b"hi");
        assert_eq!(decode_base64url("aGk=").unwrap(), b"hi");
        assert_eq!(decode_base64url("-_8").unwrap(), decode_base64url("+/8=").unwrap());
        assert!(decode_base64url("not*base64").is_none());
    }

    #[tokio::test]
    async fn search_lists_then_gets_each_message_with_a_bearer_token() {
        let fake = FakeGoogle::start(vec![
            token_route("at-1"),
            ("GET", "/users/me/messages?", 200,
             r#"{"messages":[{"id":"m1"},{"id":"m2"}],"resultSizeEstimate":2}"#.into()),
            ("GET", "/users/me/messages/m1", 200,
             serde_json::json!({"id":"m1","threadId":"t1","internalDate":"1792850400000","payload":{
                 "mimeType":"text/plain","headers":[{"name":"From","value":"a@example.com"},{"name":"Subject","value":"One"}],
                 "body":{"data": b64url("first")}}}).to_string()),
            ("GET", "/users/me/messages/m2", 200,
             serde_json::json!({"id":"m2","threadId":"t2","internalDate":"1792850300000","payload":{
                 "mimeType":"text/plain","headers":[{"name":"From","value":"b@example.com"},{"name":"Subject","value":"Two"}],
                 "body":{"data": b64url("second")}}}).to_string()),
        ])
        .await;
        let g = client_with_token(&fake, Some(stored(MAILBOX)));
        let emails = g.search(MAILBOX, "is:unread after:1792846800", 50).await.unwrap();
        assert_eq!(emails.len(), 2);
        assert_eq!(emails[0].message_id, "m1");
        assert_eq!(emails[0].body, "first");
        assert_eq!(emails[1].subject, "Two");

        let seen = fake.seen();
        let list = seen.iter().find(|s| s.line.contains("/users/me/messages?")).unwrap();
        assert!(list.line.contains("q=is%3Aunread+after%3A1792846800") || list.line.contains("q=is%3Aunread%20after%3A1792846800"), "{}", list.line);
        assert!(list.line.contains("maxResults=50"));
        assert_eq!(list.auth, "Bearer at-1");
        assert!(seen.iter().filter(|s| s.line.contains("format=full")).count() == 2);
        // One refresh for the whole search, not one per request.
        assert_eq!(seen.iter().filter(|s| s.line.contains("/token")).count(), 1);
        assert_eq!(fake.count(), 4);
    }

    #[tokio::test]
    async fn a_401_refreshes_the_access_token_once_and_retries() {
        let fake = FakeGoogle::start(vec![
            ("POST", "once:/token", 200, r#"{"access_token":"stale","expires_in":3600}"#.into()),
            ("POST", "/token", 200, r#"{"access_token":"fresh","expires_in":3600}"#.into()),
            ("GET", "once:/users/me/profile", 401, r#"{"error":{"code":401,"message":"Invalid Credentials"}}"#.into()),
            ("GET", "/users/me/profile", 200, r#"{"emailAddress":"owner@example.com"}"#.into()),
        ])
        .await;
        let g = client_with_token(&fake, Some(stored(MAILBOX)));
        assert_eq!(g.profile_email(MAILBOX).await.unwrap(), "owner@example.com");
        let auths: Vec<String> = fake
            .seen()
            .into_iter()
            .filter(|s| s.line.contains("/profile"))
            .map(|s| s.auth)
            .collect();
        assert_eq!(auths, vec!["Bearer stale", "Bearer fresh"]);
        // The refreshed token is persisted for sibling processes.
        assert_eq!(g.token(MAILBOX).unwrap().access_token.as_deref(), Some("fresh"));
    }

    #[tokio::test]
    async fn invalid_grant_latches_the_credential_as_revoked_and_names_the_mailbox() {
        let fake = FakeGoogle::start(vec![(
            "POST",
            "/token",
            400,
            r#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#.into(),
        )])
        .await;
        let g = client_with_token(&fake, Some(stored(MAILBOX)));
        let err = g.profile_email(MAILBOX).await.unwrap_err().to_string();
        assert!(err.contains("owner@example.com") && err.contains("reconnect"), "{err}");
        let latched = g.token(MAILBOX).unwrap();
        assert!(latched.is_revoked());
        assert!(latched.revoke_reason.as_deref().unwrap().contains("invalid_grant"));
        // The next call fails fast without asking Google again.
        let before = fake.count();
        let err = g.profile_email(MAILBOX).await.unwrap_err().to_string();
        assert!(err.contains("reconnect"), "{err}");
        assert_eq!(fake.count(), before);
    }

    #[tokio::test]
    async fn connect_exchanges_the_code_learns_the_mailbox_and_stores_the_grant() {
        let fake = FakeGoogle::start(vec![
            ("POST", "/token", 200,
             r#"{"access_token":"at-new","refresh_token":"rt-new","expires_in":3599,"scope":"https://www.googleapis.com/auth/gmail.modify"}"#.into()),
            ("GET", "/users/me/profile", 200, r#"{"emailAddress":"Owner@Example.com"}"#.into()),
        ])
        .await;
        let g = client_with_token(&fake, None);
        let t = g
            .connect("4/0Acode", "http://localhost:3000/oauth/google/callback")
            .await
            .unwrap();
        assert_eq!(t.email, "owner@example.com");
        assert_eq!(t.refresh_token, "rt-new");
        assert!(g.has_token("OWNER@example.com"));
        let exchange = fake.seen().into_iter().find(|s| s.line.contains("/token")).unwrap();
        assert!(exchange.body.contains("grant_type=authorization_code"));
        assert!(exchange.body.contains("code=4%2F0Acode"));
        assert!(exchange.body.contains("redirect_uri=http%3A%2F%2Flocalhost%3A3000%2Foauth%2Fgoogle%2Fcallback"));
        // The access token from the exchange is used as-is: no extra refresh.
        assert_eq!(g.access_token(MAILBOX).await.unwrap(), "at-new");
        assert_eq!(fake.count(), 2);
    }

    #[tokio::test]
    async fn disconnect_revokes_at_google_and_forgets_the_grant() {
        let fake = FakeGoogle::start(vec![("POST", "/revoke", 200, "{}".into())]).await;
        let g = client_with_token(&fake, Some(stored(MAILBOX)));
        assert!(g.disconnect(MAILBOX).await.unwrap());
        assert!(!g.has_token(MAILBOX));
        let revoke = fake.seen().into_iter().find(|s| s.line.contains("/revoke")).unwrap();
        assert_eq!(revoke.body, "token=rt-1");
        // Disconnecting an unknown mailbox is a no-op, not an error.
        assert!(!g.disconnect("nobody@example.com").await.unwrap());
    }

    #[tokio::test]
    async fn create_draft_threads_the_reply_and_returns_the_draft_id() {
        let fake = FakeGoogle::start(vec![
            token_route("at-1"),
            ("GET", "/users/me/threads/t1", 200,
             serde_json::json!({"messages":[
                 {"id":"m0","internalDate":"1","payload":{"headers":[{"name":"Message-ID","value":"<root@x>"}]}},
                 {"id":"m1","internalDate":"2","payload":{"headers":[
                     {"name":"Message-ID","value":"<latest@x>"},{"name":"References","value":"<root@x>"}]}}
             ]}).to_string()),
            ("POST", "/users/me/drafts", 200, r#"{"id":"r-draft-1","message":{"id":"dm1","threadId":"t1"}}"#.into()),
        ])
        .await;
        let g = client_with_token(&fake, Some(stored(MAILBOX)));
        let id = g
            .create_draft(
                MAILBOX,
                DraftSpec {
                    to: "a@example.com",
                    subject: "Re: thing",
                    body: "reply",
                    thread_id: Some("t1"),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(id, "r-draft-1");
        let post = fake.seen().into_iter().find(|s| s.line.starts_with("POST") && s.line.contains("/drafts")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&post.body).unwrap();
        assert_eq!(v.pointer("/message/threadId").and_then(|t| t.as_str()), Some("t1"));
        let mime = decode_raw(&post.body);
        assert!(mime.contains("In-Reply-To: <latest@x>\r\n"), "{mime}");
        assert!(mime.contains("References: <root@x> <latest@x>\r\n"), "{mime}");
        let thread_get = fake.seen().into_iter().find(|s| s.line.contains("/threads/t1")).unwrap();
        assert!(thread_get.line.contains("format=metadata"));
    }

    #[tokio::test]
    async fn create_draft_rejects_an_empty_recipient_list_before_calling_google() {
        let fake = FakeGoogle::start(vec![token_route("at-1")]).await;
        let g = client_with_token(&fake, Some(stored(MAILBOX)));
        let err = g
            .create_draft(MAILBOX, DraftSpec { to: " , ", subject: "s", body: "b", ..Default::default() })
            .await
            .unwrap_err();
        assert!(matches!(err, GmailError::Invalid(_)), "{err}");
        assert_eq!(fake.count(), 0);
    }

    #[tokio::test]
    async fn update_send_and_delete_use_the_draft_endpoints() {
        let fake = FakeGoogle::start(vec![
            token_route("at-1"),
            ("PUT", "/users/me/drafts/d1", 200, r#"{"id":"d1","message":{"id":"dm2"}}"#.into()),
            ("POST", "/users/me/drafts/send", 200, r#"{"id":"sent-99","threadId":"t9","labelIds":["SENT"]}"#.into()),
            ("DELETE", "/users/me/drafts/d1", 204, String::new()),
        ])
        .await;
        let g = client_with_token(&fake, Some(stored(MAILBOX)));
        let same = g
            .update_draft(MAILBOX, "d1", DraftSpec { to: "a@example.com", subject: "s", body: "b2", ..Default::default() })
            .await
            .unwrap();
        assert_eq!(same, "d1");
        assert_eq!(g.send_draft(MAILBOX, "d1").await.unwrap().as_deref(), Some("sent-99"));
        g.delete_draft(MAILBOX, "d1").await.unwrap();
        let send = fake.seen().into_iter().find(|s| s.line.contains("/drafts/send")).unwrap();
        assert_eq!(send.body, r#"{"id":"d1"}"#);
    }

    #[tokio::test]
    async fn resolve_thread_id_maps_a_message_and_falls_back_to_the_thread() {
        let fake = FakeGoogle::start(vec![
            token_route("at-1"),
            ("GET", "/users/me/messages/m1", 200, r#"{"id":"m1","threadId":"t1"}"#.into()),
            ("GET", "/users/me/messages/t7", 404, r#"{"error":{"code":404,"message":"Requested entity was not found."}}"#.into()),
            ("GET", "/users/me/threads/t7", 200, r#"{"id":"t7","messages":[]}"#.into()),
            ("GET", "/users/me/messages/zz", 404, "{}".into()),
            ("GET", "/users/me/threads/zz", 404, "{}".into()),
        ])
        .await;
        let g = client_with_token(&fake, Some(stored(MAILBOX)));
        assert_eq!(g.resolve_thread_id(MAILBOX, "m1").await.unwrap(), "t1");
        assert_eq!(g.resolve_thread_id(MAILBOX, "t7").await.unwrap(), "t7");
        let err = g.resolve_thread_id(MAILBOX, "zz").await.unwrap_err().to_string();
        assert!(err.contains("neither a message nor a thread"), "{err}");
    }

    #[tokio::test]
    async fn attachment_bytes_decodes_the_base64url_payload() {
        let fake = FakeGoogle::start(vec![
            token_route("at-1"),
            ("GET", "/users/me/messages/m1/attachments/ANG-1", 200,
             format!(r#"{{"size":13,"data":"{}"}}"#, b64url("%PDF-1.4 fake"))),
        ])
        .await;
        let g = client_with_token(&fake, Some(stored(MAILBOX)));
        let bytes = g.attachment_bytes(MAILBOX, "m1", "ANG-1").await.unwrap();
        assert_eq!(bytes, b"%PDF-1.4 fake");
    }

    #[tokio::test]
    async fn rate_limited_calls_back_off_and_surface_googles_message() {
        let fake = FakeGoogle::start(vec![
            token_route("at-1"),
            ("GET", "/users/me/profile", 429, r#"{"error":{"code":429,"message":"User-rate limit exceeded."}}"#.into()),
        ])
        .await;
        let g = client_with_token(&fake, Some(stored(MAILBOX)));
        let err = g.profile_email(MAILBOX).await.unwrap_err();
        assert!(matches!(err, GmailError::Google { .. }));
        assert!(err.to_string().contains("User-rate limit exceeded"), "{err}");
        // token + MAX_ATTEMPTS profile calls
        assert_eq!(fake.count(), 1 + MAX_ATTEMPTS as usize);
    }
}
