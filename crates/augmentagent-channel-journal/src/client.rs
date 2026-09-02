//! SigV4-signed AppSync GraphQL client for ShadowNote entries.
//!
//! Mirrors the house client shape (`ComposioClient` /
//! `ComposioCalendarClient`): one method per operation, shared
//! retry+backoff, `with_base_url` for mock-server tests, and the
//! [`JournalApi`] trait as the seam tests inject a fake into.
//!
//! The GraphQL documents are copied verbatim from the app's generated
//! `src/graphql/{queries,mutations}.js` so field selections stay in sync
//! with what the app itself reads — including the DataStore sync metadata
//! (`_version`, `_deleted`, `_lastChangedAt`) that makes `syncEntries`
//! delta polling and conflict-safe writes possible.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::{Duration, SystemTime};
use thiserror::Error;
use tracing::warn;

use aws_credential_types::provider::ProvideCredentials;
use aws_credential_types::provider::SharedCredentialsProvider;
use aws_sigv4::http_request::{sign, SignableBody, SignableRequest, SigningSettings};
use aws_sigv4::sign::v4;

use crate::config::JournalConfig;

const PAGE_LIMIT: u32 = 100;

const QUERY_SYNC_ENTRIES: &str = r#"query SyncEntries($filter: ModelEntryFilterInput, $limit: Int, $nextToken: String, $lastSync: AWSTimestamp) {
  syncEntries(filter: $filter, limit: $limit, nextToken: $nextToken, lastSync: $lastSync) {
    items { id ownerId createdAt content title topic bookmarked updatedAt _version _deleted _lastChangedAt owner }
    nextToken
    startedAt
  }
}"#;

const QUERY_LIST_ENTRIES: &str = r#"query ListEntries($ownerId: String, $limit: Int, $nextToken: String, $sortDirection: ModelSortDirection) {
  listEntries(ownerId: $ownerId, limit: $limit, nextToken: $nextToken, sortDirection: $sortDirection) {
    items { id ownerId createdAt content title topic bookmarked updatedAt _version _deleted _lastChangedAt owner }
    nextToken
  }
}"#;

const QUERY_GET_ENTRY: &str = r#"query GetEntry($ownerId: String!, $createdAt: AWSDateTime!) {
  getEntry(ownerId: $ownerId, createdAt: $createdAt) {
    id ownerId createdAt content title topic bookmarked updatedAt _version _deleted _lastChangedAt owner
  }
}"#;

const MUTATION_CREATE_ENTRY: &str = r#"mutation CreateEntry($input: CreateEntryInput!) {
  createEntry(input: $input) {
    id ownerId createdAt content title topic bookmarked updatedAt _version _deleted _lastChangedAt owner
  }
}"#;

/// One journal entry as the API returns it. `content` is the encrypted
/// envelope JSON (see `crypto`) — decryption is deliberately not this
/// layer's job.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub id: String,
    pub owner_id: String,
    pub created_at: String,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub topic: Option<String>,
    #[serde(default)]
    pub bookmarked: Option<bool>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default, rename = "_version")]
    pub version: Option<i64>,
    #[serde(default, rename = "_deleted")]
    pub deleted: Option<bool>,
    #[serde(default, rename = "_lastChangedAt")]
    pub last_changed_at: Option<i64>,
    #[serde(default)]
    pub owner: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryPage {
    #[serde(default)]
    pub items: Vec<Entry>,
    #[serde(default)]
    pub next_token: Option<String>,
    /// DataStore epoch-millis watermark; persist it and pass it back as
    /// `lastSync` on the next `sync_entries` call for delta polling.
    #[serde(default)]
    pub started_at: Option<i64>,
}

/// Input for the write path. `content` must already be the encrypted
/// envelope JSON from `crypto::encrypt_entry_content`.
#[derive(Debug, Clone)]
pub struct NewEntry {
    /// RFC3339 (`AWSDateTime`), also the table's sort key.
    pub created_at: String,
    pub content: String,
    pub title: Option<String>,
    pub topic: Option<String>,
}

#[derive(Debug, Error)]
pub enum JournalError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("appsync: {message}")]
    Api { message: String },
    #[error("unauthorized: {message} — check the IAM policy / custom-roles registration")]
    Unauthorized { message: String },
    #[error("credentials: {0}")]
    Credentials(String),
    #[error("signing: {0}")]
    Signing(String),
    #[error("response decode: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("store: {0}")]
    Store(String),
}

/// The seam higher layers (ingest poller, Discord write-back) depend on.
#[async_trait]
pub trait JournalApi: Send + Sync {
    /// DataStore delta query. `last_sync = None` = base sync (full scan,
    /// server-side filtered to the configured owner).
    async fn sync_entries(
        &self,
        last_sync: Option<i64>,
        next_token: Option<String>,
    ) -> Result<EntryPage, JournalError>;

    /// Partition-key list of the owner's entries (backfill path).
    async fn list_entries(&self, next_token: Option<String>) -> Result<EntryPage, JournalError>;

    async fn get_entry(&self, created_at: &str) -> Result<Option<Entry>, JournalError>;

    /// Create an entry owned by the configured user (stamps `ownerId` +
    /// `owner` so it renders in the app).
    async fn create_entry(&self, new_entry: NewEntry) -> Result<Entry, JournalError>;
}

pub struct ShadowNoteClient {
    http: reqwest::Client,
    url: String,
    region: String,
    owner_id: String,
    owner_field: String,
    credentials: SharedCredentialsProvider,
}

/// #901 — no request may hang the poller. On 2026-08-31 a sync pass sat
/// inside an unbounded request for hours, which froze the watermark and
/// turned every restart into a full replay. Connect and whole-request
/// bounds; the latter is overridable with `SHADOWNOTE_HTTP_TIMEOUT_SECS`.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

fn http_client(connect: Duration, request: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(connect)
        .timeout(request)
        .build()
        .unwrap_or_else(|e| {
            warn!("reqwest client builder failed ({e}); falling back to the default client");
            reqwest::Client::new()
        })
}

fn request_timeout_from_env() -> Duration {
    std::env::var("SHADOWNOTE_HTTP_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_REQUEST_TIMEOUT)
}

impl ShadowNoteClient {
    pub fn new(config: &JournalConfig, credentials: SharedCredentialsProvider) -> Self {
        Self {
            http: http_client(DEFAULT_CONNECT_TIMEOUT, request_timeout_from_env()),
            url: config.appsync_url.clone(),
            region: config.region.clone(),
            owner_id: config.owner_id.clone(),
            owner_field: config.owner_field.clone(),
            credentials,
        }
    }

    /// Production constructor: credentials from the standard SDK chain
    /// (env vars / profile / IMDS), so the systemd unit just sets
    /// `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`.
    pub async fn from_aws_env(config: &JournalConfig) -> Result<Self, JournalError> {
        let sdk_config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new(config.region.clone()))
            .load()
            .await;
        let credentials = sdk_config
            .credentials_provider()
            .ok_or_else(|| JournalError::Credentials("no AWS credentials provider".into()))?;
        Ok(Self::new(config, credentials))
    }

    /// Point at a mock server in tests.
    pub fn with_base_url(mut self, url: String) -> Self {
        self.url = url;
        self
    }

    /// #901 — override the connect / whole-request bounds (tests, ops tuning).
    pub fn with_timeouts(mut self, connect: Duration, request: Duration) -> Self {
        self.http = http_client(connect, request);
        self
    }

    async fn signed_request(&self, body: &str) -> Result<reqwest::Request, JournalError> {
        let creds = self
            .credentials
            .provide_credentials()
            .await
            .map_err(|e| JournalError::Credentials(e.to_string()))?;
        let identity = creds.into();
        let params: aws_sigv4::http_request::SigningParams = v4::SigningParams::builder()
            .identity(&identity)
            .region(&self.region)
            .name("appsync")
            .time(SystemTime::now())
            .settings(SigningSettings::default())
            .build()
            .map_err(|e| JournalError::Signing(e.to_string()))?
            .into();

        let parsed = reqwest::Url::parse(&self.url)
            .map_err(|e| JournalError::Signing(format!("bad AppSync URL: {e}")))?;
        let host = match (parsed.host_str(), parsed.port()) {
            (Some(h), Some(p)) => format!("{h}:{p}"),
            (Some(h), None) => h.to_string(),
            (None, _) => return Err(JournalError::Signing("URL has no host".into())),
        };
        let headers = [
            ("content-type", "application/json"),
            ("host", host.as_str()),
        ];
        let signable = SignableRequest::new(
            "POST",
            self.url.as_str(),
            headers.iter().map(|(k, v)| (*k, *v)),
            SignableBody::Bytes(body.as_bytes()),
        )
        .map_err(|e| JournalError::Signing(e.to_string()))?;
        let (instructions, _signature) = sign(signable, &params)
            .map_err(|e| JournalError::Signing(e.to_string()))?
            .into_parts();

        let mut request = http::Request::builder()
            .method("POST")
            .uri(&self.url)
            .header("content-type", "application/json")
            .body(body.to_string())
            .map_err(|e| JournalError::Signing(e.to_string()))?;
        instructions.apply_to_request_http1x(&mut request);
        reqwest::Request::try_from(request).map_err(JournalError::Http)
    }

    async fn graphql(&self, query: &str, variables: Value) -> Result<Value, JournalError> {
        let body = json!({ "query": query, "variables": variables }).to_string();

        const MAX_ATTEMPTS: u32 = 3;
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            // Re-sign each attempt: the signature embeds the request time.
            let request = self.signed_request(&body).await?;
            match self.http.execute(request).await {
                Ok(resp) => {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    if !status.is_success() {
                        if status.as_u16() == 401 || status.as_u16() == 403 {
                            return Err(JournalError::Unauthorized {
                                message: format!("{status}: {text}"),
                            });
                        }
                        let retryable = status.as_u16() == 429 || status.is_server_error();
                        if retryable && attempt < MAX_ATTEMPTS {
                            warn!(status = %status, attempt, "appsync retryable failure; backing off");
                            backoff(attempt).await;
                            continue;
                        }
                        return Err(JournalError::Api {
                            message: format!("{status}: {text}"),
                        });
                    }
                    let value: Value = serde_json::from_str(&text)?;
                    if let Some(errors) = value.get("errors").and_then(Value::as_array) {
                        if !errors.is_empty() {
                            let message = errors[0]
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("unknown GraphQL error")
                                .to_string();
                            let error_type = errors[0]
                                .get("errorType")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            if error_type.contains("Unauthorized") {
                                return Err(JournalError::Unauthorized { message });
                            }
                            return Err(JournalError::Api { message });
                        }
                    }
                    return Ok(value.get("data").cloned().unwrap_or(Value::Null));
                }
                Err(e) if attempt < MAX_ATTEMPTS && is_transient_reqwest(&e) => {
                    warn!(attempt, "appsync transport error; retrying: {e}");
                    backoff(attempt).await;
                    continue;
                }
                Err(e) => return Err(JournalError::Http(e)),
            }
        }
    }

    /// Belt-and-braces owner scoping: the IAM rule can see every user's
    /// rows, so even server-side-filtered pages are re-checked here and
    /// foreign rows dropped loudly.
    fn retain_owned(&self, mut page: EntryPage) -> EntryPage {
        let before = page.items.len();
        page.items.retain(|e| e.owner_id == self.owner_id);
        let dropped = before - page.items.len();
        if dropped > 0 {
            warn!(
                dropped,
                "syncEntries returned rows for a different ownerId; dropped (check owner scoping)"
            );
        }
        page
    }
}

#[async_trait]
impl JournalApi for ShadowNoteClient {
    async fn sync_entries(
        &self,
        last_sync: Option<i64>,
        next_token: Option<String>,
    ) -> Result<EntryPage, JournalError> {
        let data = self
            .graphql(
                QUERY_SYNC_ENTRIES,
                json!({
                    "filter": { "ownerId": { "eq": self.owner_id } },
                    "limit": PAGE_LIMIT,
                    "nextToken": next_token,
                    "lastSync": last_sync,
                }),
            )
            .await?;
        let page: EntryPage = serde_json::from_value(
            data.get("syncEntries").cloned().unwrap_or(Value::Null),
        )?;
        Ok(self.retain_owned(page))
    }

    async fn list_entries(&self, next_token: Option<String>) -> Result<EntryPage, JournalError> {
        let data = self
            .graphql(
                QUERY_LIST_ENTRIES,
                json!({
                    "ownerId": self.owner_id,
                    "limit": PAGE_LIMIT,
                    "nextToken": next_token,
                    "sortDirection": "DESC",
                }),
            )
            .await?;
        let page: EntryPage = serde_json::from_value(
            data.get("listEntries").cloned().unwrap_or(Value::Null),
        )?;
        Ok(self.retain_owned(page))
    }

    async fn get_entry(&self, created_at: &str) -> Result<Option<Entry>, JournalError> {
        let data = self
            .graphql(
                QUERY_GET_ENTRY,
                json!({ "ownerId": self.owner_id, "createdAt": created_at }),
            )
            .await?;
        match data.get("getEntry") {
            None | Some(Value::Null) => Ok(None),
            Some(v) => Ok(Some(serde_json::from_value(v.clone())?)),
        }
    }

    async fn create_entry(&self, new_entry: NewEntry) -> Result<Entry, JournalError> {
        let input = json!({
            "id": uuid::Uuid::new_v4().to_string(),
            "ownerId": self.owner_id,
            "owner": self.owner_field,
            "createdAt": new_entry.created_at,
            "content": new_entry.content,
            "title": new_entry.title,
            "topic": new_entry.topic,
            "bookmarked": false,
        });
        let data = self
            .graphql(MUTATION_CREATE_ENTRY, json!({ "input": input }))
            .await?;
        let created = data
            .get("createEntry")
            .cloned()
            .ok_or_else(|| JournalError::Api {
                message: "createEntry returned no data".into(),
            })?;
        Ok(serde_json::from_value(created)?)
    }
}

fn is_transient_reqwest(e: &reqwest::Error) -> bool {
    e.is_timeout() || e.is_connect() || e.is_request()
}

async fn backoff(attempt: u32) {
    let ms = 250u64.saturating_mul(1u64 << attempt.min(4));
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_credential_types::Credentials;

    fn test_client(base_url: &str) -> ShadowNoteClient {
        let config = JournalConfig {
            appsync_url: format!("{base_url}/graphql"),
            owner_id: "owner-123".into(),
            owner_field: "cognito-sub-123".into(),
            kms_key_arn: None,
            region: "us-east-1".into(),
        };
        let creds = Credentials::new("AKIDTEST", "SECRETTEST", None, None, "test");
        ShadowNoteClient::new(&config, SharedCredentialsProvider::new(creds))
    }

    fn entry_json(owner_id: &str, title: &str) -> Value {
        json!({
            "id": "e1", "ownerId": owner_id, "createdAt": "2026-07-01T08:00:00.000Z",
            "content": "{\"ciphertext\":\"x\",\"ciphertextDEK\":\"y\"}",
            "title": title, "topic": "Journal", "bookmarked": false,
            "updatedAt": "2026-07-01T08:00:00.000Z",
            "_version": 1, "_deleted": null, "_lastChangedAt": 1751356800000i64,
            "owner": "cognito-sub-123"
        })
    }

    #[tokio::test]
    async fn sync_entries_parses_page_and_signs_request() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/graphql")
            // SigV4 must be applied — AppSync rejects unsigned calls, so
            // catching a missing Authorization header in tests is cheap
            // insurance against a signing regression.
            .match_header(
                "authorization",
                mockito::Matcher::Regex("^AWS4-HMAC-SHA256 .*appsync.*".into()),
            )
            .match_header("x-amz-date", mockito::Matcher::Any)
            .with_status(200)
            .with_body(
                json!({ "data": { "syncEntries": {
                    "items": [entry_json("owner-123", "kept")],
                    "nextToken": "tok-2",
                    "startedAt": 1751356800123i64
                }}})
                .to_string(),
            )
            .create_async()
            .await;

        let client = test_client(&server.url());
        let page = client.sync_entries(Some(0), None).await.unwrap();
        mock.assert_async().await;
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].title.as_deref(), Some("kept"));
        assert_eq!(page.next_token.as_deref(), Some("tok-2"));
        assert_eq!(page.started_at, Some(1751356800123));
    }

    #[tokio::test]
    async fn foreign_owner_rows_are_dropped_client_side() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/graphql")
            .with_status(200)
            .with_body(
                json!({ "data": { "syncEntries": {
                    "items": [entry_json("owner-123", "mine"), entry_json("someone-else", "theirs")],
                    "nextToken": null, "startedAt": null
                }}})
                .to_string(),
            )
            .create_async()
            .await;

        let page = test_client(&server.url())
            .sync_entries(None, None)
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].title.as_deref(), Some("mine"));
    }

    /// #901 — the poller froze for hours on 2026-08-31 because nothing
    /// bounded a request. A server that accepts and never answers must
    /// surface as a timeout error, quickly.
    #[tokio::test]
    async fn request_times_out_against_a_silent_server() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hold = tokio::spawn(async move {
            let mut held = Vec::new();
            loop {
                if let Ok((sock, _)) = listener.accept().await {
                    held.push(sock);
                }
            }
        });
        let client = test_client(&format!("http://{addr}"))
            .with_timeouts(Duration::from_millis(200), Duration::from_millis(300));
        let started = std::time::Instant::now();
        let err = client.sync_entries(None, None).await.unwrap_err();
        assert!(
            matches!(&err, JournalError::Http(e) if e.is_timeout()),
            "expected a timeout, got {err:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "took {:?}",
            started.elapsed()
        );
        hold.abort();
    }

    #[tokio::test]
    async fn graphql_errors_surface_as_api_errors() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/graphql")
            .with_status(200)
            .with_body(
                json!({ "data": null, "errors": [
                    { "errorType": "Unauthorized", "message": "Not Authorized to access syncEntries" }
                ]})
                .to_string(),
            )
            .create_async()
            .await;

        let err = test_client(&server.url())
            .sync_entries(None, None)
            .await
            .unwrap_err();
        assert!(matches!(err, JournalError::Unauthorized { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn create_entry_stamps_owner_fields() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("POST", "/graphql")
            .match_body(mockito::Matcher::AllOf(vec![
                mockito::Matcher::Regex("\"ownerId\":\"owner-123\"".into()),
                mockito::Matcher::Regex("\"owner\":\"cognito-sub-123\"".into()),
                mockito::Matcher::Regex("CreateEntry".into()),
            ]))
            .with_status(200)
            .with_body(
                json!({ "data": { "createEntry": entry_json("owner-123", "from agent") } })
                    .to_string(),
            )
            .create_async()
            .await;

        let created = test_client(&server.url())
            .create_entry(NewEntry {
                created_at: "2026-07-11T21:00:00.000Z".into(),
                content: "{\"ciphertext\":\"c\",\"ciphertextDEK\":\"d\"}".into(),
                title: Some("from agent".into()),
                topic: Some("Journal".into()),
            })
            .await
            .unwrap();
        mock.assert_async().await;
        assert_eq!(created.owner_id, "owner-123");
    }

    #[tokio::test]
    async fn get_entry_null_is_none() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/graphql")
            .with_status(200)
            .with_body(json!({ "data": { "getEntry": null } }).to_string())
            .create_async()
            .await;

        let got = test_client(&server.url())
            .get_entry("2026-01-01T00:00:00.000Z")
            .await
            .unwrap();
        assert!(got.is_none());
    }
}
