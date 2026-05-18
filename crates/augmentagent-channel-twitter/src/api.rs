//! X / Twitter internal web API client.
//!
//! Narrow scope:
//! - **#15** friend-post engagement: list a tracked user's recent tweets
//!   (`UserTweets` GraphQL) + post a reply (`CreateTweet` GraphQL).
//! - **#16** DM channel: read the DM inbox
//!   (`/i/api/1.1/dm/inbox_initial_state.json`) + send a DM
//!   (`/i/api/1.1/dm/new2.json`).
//!
//! REQUIRES LIVE OPERATOR VALIDATION — the GraphQL `queryId` fragments and
//! `features` maps below are reconstructed from public knowledge, not a live
//! capture. The fragile bits (rotating hashed queryIds, the boolean
//! `features` map X 400s on if stale, `x-client-transaction-id` derivation)
//! are documented in `docs/twitter-protocol.md`. The static fallbacks here
//! are the recovery path; a captured id should be cached in the
//! `twitter_query_ids` store table and wins over the static default.
//!
//! All live network calls are gated by the caller: the channel layer only
//! reaches `reply_to_tweet` / `send_dm` after a Discord approval, and the
//! posting client (`client.rs`) gates `create` behind a dry-run flag + the
//! hard 15/day quota preflight.

use async_trait::async_trait;
use serde::Deserialize;
use thiserror::Error;
use uuid::Uuid;

use crate::auth::TwitterAuth;
use crate::types::{Tweet, TwitterDm};

/// Static fallback `queryId`s. X rotates these on its web deploys (~every
/// 2-6 weeks). The store's `twitter_query_ids` cache, when populated from a
/// capture, takes precedence. Operation name + a known-shape hashed id;
/// override at runtime via env (see `query_id_for`).
pub const DEFAULT_USER_TWEETS_QUERY_ID: &str = "E3opETHurmVJflFsUBVuUQ";
pub const DEFAULT_CREATE_TWEET_QUERY_ID: &str = "SoVnbfCycZ7fkQT1yYP3Lw";

/// Origin for every X internal-API call. Defaults to `https://x.com`; the
/// `AUGMENTAGENT_TWITTER_BASE_URL` override lets the wiremock-backed
/// validation/error tests + the `twitter validate` harness's self-test point
/// the client at a local mock with no network call. Production never sets it.
pub fn base_url() -> String {
    std::env::var("AUGMENTAGENT_TWITTER_BASE_URL")
        .unwrap_or_else(|_| "https://x.com".to_string())
}

/// Conservative default backoff (seconds) when X 429s without a parseable
/// `Retry-After` or `x-rate-limit-reset`.
pub const DEFAULT_RATE_LIMIT_BACKOFF_SECS: u64 = 15 * 60;

/// A resolver for GraphQL `queryId`s. The client owns the
/// store→env→default fallback chain through this trait so it stays
/// store-free and trivially mockable (the store-backed impl lives behind a
/// blanket `dyn` in the channel layer). On a [`TwitterError::QueryIdRotated`]
/// the client asks the resolver to *skip* the failing id and produce the
/// next candidate, so a single redeploy doesn't wedge the channel.
pub trait QueryIdResolver: Send + Sync {
    /// The candidate chain for `op` (a GraphQL operation name like
    /// `UserTweets`), most-trusted first. The client tries them in order on
    /// a `QueryIdRotated` until one resolves or the chain is exhausted.
    fn candidates(&self, op: &str) -> Vec<String>;
}

/// The default, store-free resolver: env override → static default. Mirrors
/// the legacy [`TwitterClient::query_id_for`] behavior but exposes the full
/// chain so the client can fall through on rotation.
pub struct EnvQueryIdResolver;

impl QueryIdResolver for EnvQueryIdResolver {
    fn candidates(&self, op: &str) -> Vec<String> {
        let default = match op {
            "UserTweets" => DEFAULT_USER_TWEETS_QUERY_ID,
            "CreateTweet" => DEFAULT_CREATE_TWEET_QUERY_ID,
            _ => "",
        };
        let env_key = format!(
            "AUGMENTAGENT_TWITTER_{}_QUERY_ID",
            op_to_env(op)
        );
        let mut out = Vec::new();
        if let Ok(v) = std::env::var(&env_key) {
            if !v.is_empty() {
                out.push(v);
            }
        }
        if !default.is_empty() && !out.iter().any(|c| c == default) {
            out.push(default.to_string());
        }
        out
    }
}

/// `UserTweets` → `USER_TWEETS`, `CreateTweet` → `CREATE_TWEET`. Splits on
/// camel-case humps for the env-var convention used since #79.
fn op_to_env(op: &str) -> String {
    let mut s = String::with_capacity(op.len() + 4);
    for (i, c) in op.chars().enumerate() {
        if c.is_ascii_uppercase() && i != 0 {
            s.push('_');
        }
        s.push(c.to_ascii_uppercase());
    }
    s
}

/// The boolean `features` map X requires on `UserTweets` / `CreateTweet`
/// GraphQL calls. Stale entries here are the #1 cause of HTTP 400 from X;
/// REQUIRES LIVE OPERATOR VALIDATION (see docs/twitter-protocol.md §2,§3).
fn graphql_features() -> serde_json::Value {
    serde_json::json!({
        "rweb_tipjar_consumption_enabled": true,
        "responsive_web_graphql_exclude_directive_enabled": true,
        "verified_phone_label_enabled": false,
        "creator_subscriptions_tweet_preview_api_enabled": true,
        "responsive_web_graphql_timeline_navigation_enabled": true,
        "responsive_web_graphql_skip_user_profile_image_extensions_enabled": false,
        "communities_web_enable_tweet_community_results_fetch": true,
        "c9s_tweet_anatomy_moderator_badge_enabled": true,
        "articles_preview_enabled": true,
        "tweetypie_unmention_optimization_enabled": true,
        "responsive_web_edit_tweet_api_enabled": true,
        "graphql_is_translatable_rweb_tweet_is_translatable_enabled": true,
        "view_counts_everywhere_api_enabled": true,
        "longform_notetweets_consumption_enabled": true,
        "responsive_web_twitter_article_tweet_consumption_enabled": true,
        "tweet_awards_web_tipping_enabled": false,
        "creator_subscriptions_quote_tweet_preview_enabled": false,
        "freedom_of_speech_not_reach_fetch_enabled": true,
        "standardized_nudges_misinfo": true,
        "tweet_with_visibility_results_prefer_gql_limited_actions_policy_enabled": true,
        "rweb_video_timestamps_enabled": true,
        "longform_notetweets_rich_text_read_enabled": true,
        "longform_notetweets_inline_media_enabled": true,
        "responsive_web_enhance_cards_enabled": false
    })
}

#[derive(Debug, Error)]
pub enum TwitterError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    /// Terminal session failure (401/403). The caller must halt polling and
    /// re-harvest cookies — backoff does not help here.
    #[error("auth expired (401/403); re-run `augmentagent twitter login`")]
    AuthExpired,
    /// HTTP 429 — rate limited. `retry_after_secs` is the best estimate of
    /// when it's safe to retry (from `Retry-After`, else `x-rate-limit-reset`,
    /// else a conservative default). Transient: the caller may back off and
    /// retry rather than treating it as terminal.
    #[error("rate limited on {op}; retry after ~{retry_after_secs}s (limit {limit:?}, remaining {remaining:?})")]
    RateLimited {
        op: String,
        retry_after_secs: u64,
        limit: Option<u64>,
        remaining: Option<u64>,
    },
    /// HTTP 404/400 on a GraphQL op — the classic signature of a rotated
    /// `queryId` (X redeployed and the hashed id we used no longer resolves).
    /// Recoverable by re-resolving the id from a fresher source (capture /
    /// env / a new default) and retrying — distinct from a hard API error.
    #[error("queryId rotated on {op} (status {status}); refresh the queryId — see docs/twitter-protocol.md §2")]
    QueryIdRotated { op: String, status: u16 },
    /// The HTTP call succeeded (2xx) but the body didn't match the documented
    /// shape — yielded zero parsed records from a non-empty payload. The raw
    /// body is logged at WARN before this is returned so the operator can
    /// diff it against the spec; `body_excerpt` is a short prefix for the
    /// error chain.
    #[error("schema drift on {op}: parsed 0 records from a {body_len}B body; raw logged at WARN. excerpt: {body_excerpt}")]
    SchemaDrift {
        op: String,
        body_len: usize,
        body_excerpt: String,
    },
    #[error("graphql/{op}: {status}: {body}")]
    Api {
        op: String,
        status: u16,
        body: String,
    },
    #[error("decode: {0}")]
    Decode(String),
    #[error("config: {0}")]
    Config(String),
}

impl TwitterError {
    /// True for errors where a bounded retry (after [`Self::backoff`]) is
    /// sane. `AuthExpired` is terminal; `QueryIdRotated` is recoverable only
    /// after the caller refreshes the id, so it is *not* a blind-retry case.
    pub fn is_transient(&self) -> bool {
        matches!(self, TwitterError::RateLimited { .. } | TwitterError::Http(_))
    }

    /// Suggested backoff before a retry of a [`Self::is_transient`] error.
    /// Honors the server's retry hint on `RateLimited`; otherwise an
    /// exponential schedule capped at 5min keyed off `attempt` (0-based).
    pub fn backoff(&self, attempt: u32) -> std::time::Duration {
        use std::time::Duration;
        if let TwitterError::RateLimited {
            retry_after_secs, ..
        } = self
        {
            // Respect the server hint but never sleep more than 15min.
            return Duration::from_secs((*retry_after_secs).min(15 * 60));
        }
        // 2^attempt seconds, jitter-free, capped at 300s.
        let secs = 1u64.checked_shl(attempt).unwrap_or(u64::MAX).min(300);
        Duration::from_secs(secs)
    }
}

/// Friend-post engagement surface (#15).
#[async_trait]
pub trait TwitterApi: Send + Sync {
    /// Most-recent tweets authored by `user_id`. `since_id`, when set, asks
    /// the caller's filter to drop anything `<= since_id` (X's GraphQL has
    /// no server-side since param on this op, so we filter client-side).
    async fn fetch_user_tweets(
        &self,
        user_id: &str,
        since_id: Option<&str>,
    ) -> Result<Vec<Tweet>, TwitterError>;

    /// Post a reply to `tweet_id`. Returns the new tweet's rest_id.
    async fn reply_to_tweet(
        &self,
        tweet_id: &str,
        text: &str,
    ) -> Result<String, TwitterError>;

    /// Read the DM inbox. `cursor` paginates (X returns a `min_entry_id`
    /// style cursor; `None` = newest page). (#16)
    async fn fetch_dm_inbox(
        &self,
        cursor: Option<&str>,
    ) -> Result<Vec<TwitterDm>, TwitterError>;

    /// Send a DM into an existing conversation. Returns the new event id. (#16)
    async fn send_dm(
        &self,
        conversation_id: &str,
        text: &str,
    ) -> Result<String, TwitterError>;
}

pub struct TwitterClient {
    http: reqwest::Client,
    auth: TwitterAuth,
    resolver: std::sync::Arc<dyn QueryIdResolver>,
}

impl TwitterClient {
    pub fn new(auth: TwitterAuth) -> Self {
        Self::with_resolver(auth, std::sync::Arc::new(EnvQueryIdResolver))
    }

    /// Construct with a custom [`QueryIdResolver`] — used by the channel to
    /// inject the store-backed (cache → env → default) chain, and by tests
    /// to feed a deterministic rotation chain.
    pub fn with_resolver(
        auth: TwitterAuth,
        resolver: std::sync::Arc<dyn QueryIdResolver>,
    ) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(auth.user_agent.clone())
            .build()
            .expect("reqwest client build");
        Self {
            http,
            auth,
            resolver,
        }
    }

    /// Resolve a queryId for an operation: env override → static default.
    /// Retained for back-compat with #79 callers; new code should go through
    /// the [`QueryIdResolver`] chain (rotation-tolerant).
    pub fn query_id_for(op: &str, default: &str) -> String {
        let env_key = format!("AUGMENTAGENT_TWITTER_{}_QUERY_ID", op.to_uppercase());
        std::env::var(env_key).unwrap_or_else(|_| default.to_string())
    }

    /// Ordered queryId candidates for `op` from the injected resolver, with
    /// the static default appended as a last-ditch entry so the chain is
    /// never empty even if the resolver returns nothing.
    fn query_id_chain(&self, op: &str) -> Vec<String> {
        let mut chain = self.resolver.candidates(op);
        let default = match op {
            "UserTweets" => DEFAULT_USER_TWEETS_QUERY_ID,
            "CreateTweet" => DEFAULT_CREATE_TWEET_QUERY_ID,
            _ => "",
        };
        if !default.is_empty() && !chain.iter().any(|c| c == default) {
            chain.push(default.to_string());
        }
        chain
    }

    fn base_headers(&self) -> Result<reqwest::header::HeaderMap, TwitterError> {
        use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
        let mut h = HeaderMap::new();
        let mut set = |name: &'static str, val: String| -> Result<(), TwitterError> {
            let name = HeaderName::from_static(name);
            let value = HeaderValue::from_str(&val)
                .map_err(|e| TwitterError::Config(format!("{name}: {e}")))?;
            h.insert(name, value);
            Ok(())
        };
        set("authorization", self.auth.authorization())?;
        set("cookie", self.auth.cookie_header())?;
        set(
            "x-csrf-token",
            self.auth
                .csrf_token()
                .map_err(|e| TwitterError::Config(e.to_string()))?,
        )?;
        set("x-twitter-auth-type", "OAuth2Session".into())?;
        set("x-twitter-active-user", "yes".into())?;
        set("x-twitter-client-language", "en".into())?;
        // Best-effort anti-automation header. Full client-side derivation
        // REQUIRES LIVE OPERATOR VALIDATION (docs/twitter-protocol.md §1).
        set("x-client-transaction-id", gen_client_transaction_id())?;
        set("content-type", "application/json".into())?;
        set("accept", "*/*".into())?;
        set("referer", "https://x.com/".into())?;
        set("origin", "https://x.com".into())?;
        Ok(h)
    }

    /// Classify a non-2xx response into a typed error. `headers` is consulted
    /// for the rate-limit window on 429. `graphql` flags GraphQL ops, where a
    /// 400/404 is the queryId-rotation signature (REST DM endpoints 400 for
    /// other reasons, so they fall through to the generic `Api` error).
    fn map_status(
        op: &str,
        status: reqwest::StatusCode,
        headers: &reqwest::header::HeaderMap,
        body: String,
        graphql: bool,
    ) -> TwitterError {
        let code = status.as_u16();
        if code == 401 || code == 403 {
            return TwitterError::AuthExpired;
        }
        if code == 429 {
            let (retry_after_secs, limit, remaining) = parse_rate_limit(headers);
            return TwitterError::RateLimited {
                op: op.into(),
                retry_after_secs,
                limit,
                remaining,
            };
        }
        if graphql && (code == 404 || code == 400) {
            // 404 = unknown queryId path; 400 on a GraphQL op is most often a
            // stale id/features map. Both are the rotation signature — the
            // caller walks the next candidate rather than hard-failing.
            return TwitterError::QueryIdRotated {
                op: op.into(),
                status: code,
            };
        }
        TwitterError::Api {
            op: op.into(),
            status: code,
            body,
        }
    }
}

/// Pull `(retry_after_secs, x-rate-limit-limit, x-rate-limit-remaining)` off
/// a 429 response. Priority for the retry estimate:
/// 1. `retry-after` (RFC 7231 delta-seconds — X sends this on hard limits)
/// 2. `x-rate-limit-reset` (unix seconds) minus now, floored at 1s
/// 3. [`DEFAULT_RATE_LIMIT_BACKOFF_SECS`]
fn parse_rate_limit(
    headers: &reqwest::header::HeaderMap,
) -> (u64, Option<u64>, Option<u64>) {
    let h = |name: &str| -> Option<u64> {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
    };
    let limit = h("x-rate-limit-limit");
    let remaining = h("x-rate-limit-remaining");
    let retry = if let Some(ra) = h("retry-after") {
        ra
    } else if let Some(reset) = h("x-rate-limit-reset") {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        reset.saturating_sub(now).max(1)
    } else {
        DEFAULT_RATE_LIMIT_BACKOFF_SECS
    };
    (retry, limit, remaining)
}

/// Emit the raw body at WARN and build a [`TwitterError::SchemaDrift`]. Called
/// when a 2xx response parsed to zero records — so an X web deploy that
/// reshapes the JSON surfaces loudly (with the body to diff) instead of the
/// channel silently going quiet.
fn schema_drift(op: &str, body: &str) -> TwitterError {
    tracing::warn!(
        op,
        body_len = body.len(),
        raw_body = %body,
        "twitter schema drift: 2xx response parsed to 0 records — \
         X may have reshaped the response; diff against docs/twitter-protocol.md"
    );
    let excerpt: String = body.chars().take(200).collect();
    TwitterError::SchemaDrift {
        op: op.into(),
        body_len: body.len(),
        body_excerpt: excerpt,
    }
}

/// Heuristic: does this 2xx body look like a populated timeline/inbox (so an
/// empty parse is genuinely "no new items") vs. an error/shape we don't
/// recognize (schema drift)? X returns `{ "errors": [...] }` with a 200 for
/// some GraphQL faults, and a reshaped success body won't carry our anchor
/// keys. We only flag drift when the body is non-trivial AND lacks every
/// known anchor for the op.
fn looks_like_drift(op: &str, body: &str) -> bool {
    if body.trim().len() < 16 {
        return false; // genuinely-empty / `{}` style — treat as "no items"
    }
    let anchors: &[&str] = match op {
        "UserTweets" => &["timeline", "instructions", "\"errors\"", "tweet_results"],
        "dm_inbox" => &["inbox_initial_state", "entries", "\"errors\"", "conversations"],
        _ => return false,
    };
    !anchors.iter().any(|a| body.contains(a))
}

#[async_trait]
impl TwitterApi for TwitterClient {
    async fn fetch_user_tweets(
        &self,
        user_id: &str,
        since_id: Option<&str>,
    ) -> Result<Vec<Tweet>, TwitterError> {
        let variables = serde_json::json!({
            "userId": user_id,
            "count": 20,
            "includePromotedContent": false,
            "withQuickPromoteEligibilityTweetFields": false,
            "withVoice": true,
            "withV2Timeline": true,
        });
        let features = graphql_features().to_string();
        let vars = variables.to_string();

        // Walk the queryId chain: on a rotation signal (404/400) drop to the
        // next candidate; on any other outcome (success or hard error) stop.
        let chain = self.query_id_chain("UserTweets");
        let mut last_rotation: Option<TwitterError> = None;
        for (i, qid) in chain.iter().enumerate() {
            let url = format!(
                "{}/i/api/graphql/{qid}/UserTweets?variables={}&features={}",
                base_url(),
                urlencoding(&vars),
                urlencoding(&features),
            );
            let resp = self
                .http
                .get(&url)
                .headers(self.base_headers()?)
                .send()
                .await?;
            let status = resp.status();
            if !status.is_success() {
                let headers = resp.headers().clone();
                let body = resp.text().await.unwrap_or_default();
                let err = Self::map_status("UserTweets", status, &headers, body, true);
                if matches!(err, TwitterError::QueryIdRotated { .. }) {
                    tracing::warn!(
                        op = "UserTweets",
                        attempted_query_id = %qid,
                        next = i + 1 < chain.len(),
                        "twitter queryId rotated; trying next candidate"
                    );
                    last_rotation = Some(err);
                    continue;
                }
                return Err(err);
            }
            let raw = resp
                .text()
                .await
                .map_err(|e| TwitterError::Decode(format!("UserTweets body: {e}")))?;
            let payload: serde_json::Value = serde_json::from_str(&raw)
                .map_err(|e| TwitterError::Decode(format!("UserTweets json: {e}")))?;
            let tweets = parse_user_tweets(&payload, user_id);
            if tweets.is_empty() && looks_like_drift("UserTweets", &raw) {
                return Err(schema_drift("UserTweets", &raw));
            }
            return Ok(filter_since(tweets, since_id));
        }
        // Exhausted every candidate, all rotated.
        Err(last_rotation.unwrap_or(TwitterError::QueryIdRotated {
            op: "UserTweets".into(),
            status: 404,
        }))
    }

    async fn reply_to_tweet(
        &self,
        tweet_id: &str,
        text: &str,
    ) -> Result<String, TwitterError> {
        let chain = self.query_id_chain("CreateTweet");
        let mut last_rotation: Option<TwitterError> = None;
        for (i, qid) in chain.iter().enumerate() {
            let url =
                format!("{}/i/api/graphql/{qid}/CreateTweet", base_url());
            let body = serde_json::json!({
                "variables": {
                    "tweet_text": text,
                    "reply": { "in_reply_to_tweet_id": tweet_id, "exclude_reply_user_ids": [] },
                    "dark_request": false,
                    "media": { "media_entities": [], "possibly_sensitive": false },
                    "semantic_annotation_ids": [],
                },
                "features": graphql_features(),
                "queryId": qid,
            });
            let resp = self
                .http
                .post(&url)
                .headers(self.base_headers()?)
                .body(serde_json::to_vec(&body).expect("serialize CreateTweet"))
                .send()
                .await?;
            let status = resp.status();
            if !status.is_success() {
                let headers = resp.headers().clone();
                let body = resp.text().await.unwrap_or_default();
                let err = Self::map_status("CreateTweet", status, &headers, body, true);
                if matches!(err, TwitterError::QueryIdRotated { .. }) {
                    tracing::warn!(
                        op = "CreateTweet",
                        attempted_query_id = %qid,
                        next = i + 1 < chain.len(),
                        "twitter queryId rotated; trying next candidate"
                    );
                    last_rotation = Some(err);
                    continue;
                }
                return Err(err);
            }
            let v: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| TwitterError::Decode(format!("CreateTweet json: {e}")))?;
            return Ok(find_string_field(&v, "rest_id").unwrap_or_default());
        }
        Err(last_rotation.unwrap_or(TwitterError::QueryIdRotated {
            op: "CreateTweet".into(),
            status: 404,
        }))
    }

    async fn fetch_dm_inbox(
        &self,
        cursor: Option<&str>,
    ) -> Result<Vec<TwitterDm>, TwitterError> {
        let mut url = format!(
            "{}/i/api/1.1/dm/inbox_initial_state.json\
             ?nsfw_filtering_enabled=false&filter_low_quality=false\
             &include_quality=all&dm_secret_conversations_enabled=false",
            base_url(),
        );
        if let Some(c) = cursor {
            url.push_str(&format!("&max_id={}", urlencoding(c)));
        }
        let resp = self
            .http
            .get(&url)
            .headers(self.base_headers()?)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let headers = resp.headers().clone();
            let body = resp.text().await.unwrap_or_default();
            return Err(Self::map_status("dm_inbox", status, &headers, body, false));
        }
        let raw = resp
            .text()
            .await
            .map_err(|e| TwitterError::Decode(format!("dm_inbox body: {e}")))?;
        let payload: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|e| TwitterError::Decode(format!("dm_inbox json: {e}")))?;
        let dms = parse_dm_inbox(&payload);
        if dms.is_empty() && looks_like_drift("dm_inbox", &raw) {
            return Err(schema_drift("dm_inbox", &raw));
        }
        Ok(dms)
    }

    async fn send_dm(
        &self,
        conversation_id: &str,
        text: &str,
    ) -> Result<String, TwitterError> {
        let url = format!("{}/i/api/1.1/dm/new2.json", base_url());
        let body = serde_json::json!({
            "conversation_id": conversation_id,
            "recipient_ids": false,
            "request_id": Uuid::new_v4().to_string(),
            "text": text,
            "cards_platform": "Web-12",
            "include_cards": 1,
            "include_quote_count": true,
            "dm_users": false,
        });
        let resp = self
            .http
            .post(&url)
            .headers(self.base_headers()?)
            .body(serde_json::to_vec(&body).expect("serialize new2"))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let headers = resp.headers().clone();
            let body = resp.text().await.unwrap_or_default();
            return Err(Self::map_status("dm_new2", status, &headers, body, false));
        }
        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| TwitterError::Decode(format!("dm_new2 json: {e}")))?;
        Ok(find_string_field(&v, "id").unwrap_or_default())
    }
}

// --- helpers ---

/// Minimal percent-encoding for GraphQL query-string params. We only need the
/// JSON punctuation escaped; alphanumerics + a handful of safe chars pass.
fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Best-effort `x-client-transaction-id`. The real value is derived from a
/// per-page-load animation/key the X bundle computes; replicating it
/// REQUIRES LIVE OPERATOR VALIDATION. Many GraphQL ops still accept a
/// plausible base64-ish opaque value; we send one rather than omit the
/// header entirely (omission is a harder reject on newer deploys).
fn gen_client_transaction_id() -> String {
    let raw = Uuid::new_v4();
    let bytes = raw.as_bytes();
    // url-safe-ish base64 without bringing in a base64 crate.
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut s = String::new();
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        s.push(A[((n >> 18) & 63) as usize] as char);
        s.push(A[((n >> 12) & 63) as usize] as char);
        s.push(A[((n >> 6) & 63) as usize] as char);
        s.push(A[(n & 63) as usize] as char);
    }
    s
}

fn filter_since(tweets: Vec<Tweet>, since_id: Option<&str>) -> Vec<Tweet> {
    match since_id {
        None => tweets,
        Some(since) => tweets
            .into_iter()
            // Numeric snowflake compare; fall back to string if non-numeric.
            .filter(|t| match (t.rest_id.parse::<u128>(), since.parse::<u128>()) {
                (Ok(a), Ok(b)) => a > b,
                _ => t.rest_id.as_str() > since,
            })
            .collect(),
    }
}

fn find_string_field(v: &serde_json::Value, key: &str) -> Option<String> {
    match v {
        serde_json::Value::Object(m) => {
            if let Some(serde_json::Value::String(s)) = m.get(key) {
                if !s.is_empty() {
                    return Some(s.clone());
                }
            }
            for (_, vv) in m {
                if let Some(s) = find_string_field(vv, key) {
                    return Some(s);
                }
            }
            None
        }
        serde_json::Value::Array(a) => {
            a.iter().find_map(|vv| find_string_field(vv, key))
        }
        _ => None,
    }
}

/// X's `created_at` is the legacy Twitter format:
/// `Wed Oct 10 20:19:24 +0000 2018`. Hand-parsed (the workspace `time` dep
/// doesn't enable the `macros`/format-description-string features) into
/// epoch ms; 0 on any malformed input.
fn twitter_time_to_ms(s: &str) -> i64 {
    use time::{Date, Month, OffsetDateTime, Time, UtcOffset};
    // tokens: [weekday, mon, day, HH:MM:SS, +ZZZZ, year]
    let p: Vec<&str> = s.split_whitespace().collect();
    if p.len() != 6 {
        return 0;
    }
    let month = match p[1] {
        "Jan" => Month::January,
        "Feb" => Month::February,
        "Mar" => Month::March,
        "Apr" => Month::April,
        "May" => Month::May,
        "Jun" => Month::June,
        "Jul" => Month::July,
        "Aug" => Month::August,
        "Sep" => Month::September,
        "Oct" => Month::October,
        "Nov" => Month::November,
        "Dec" => Month::December,
        _ => return 0,
    };
    let (Ok(day), Ok(year)) = (p[2].parse::<u8>(), p[5].parse::<i32>()) else {
        return 0;
    };
    let hms: Vec<&str> = p[3].split(':').collect();
    if hms.len() != 3 {
        return 0;
    }
    let (Ok(h), Ok(mi), Ok(se)) = (
        hms[0].parse::<u8>(),
        hms[1].parse::<u8>(),
        hms[2].parse::<u8>(),
    ) else {
        return 0;
    };
    // Offset like +0000 / -0700.
    let off = p[4];
    let sign = match off.as_bytes().first() {
        Some(b'+') => 1,
        Some(b'-') => -1,
        _ => return 0,
    };
    let (Ok(oh), Ok(om)) = (off[1..3].parse::<i8>(), off[3..5].parse::<i8>()) else {
        return 0;
    };
    let Ok(offset) = UtcOffset::from_hms(sign * oh, sign * om, 0) else {
        return 0;
    };
    let Ok(date) = Date::from_calendar_date(year, month, day) else {
        return 0;
    };
    let Ok(tm) = Time::from_hms(h, mi, se) else {
        return 0;
    };
    OffsetDateTime::new_in_offset(date, tm, offset).unix_timestamp() * 1000
}

/// Walk a `UserTweets` timeline response and pull author-owned tweets.
/// Tolerant: unknown shapes are skipped, never panic. The exact instruction
/// nesting REQUIRES LIVE OPERATOR VALIDATION; this handles the documented
/// `data.user.result.timeline_v2.timeline.instructions[].entries[]` shape.
fn parse_user_tweets(payload: &serde_json::Value, author_id: &str) -> Vec<Tweet> {
    #[derive(Deserialize)]
    struct LegacyTweet {
        full_text: Option<String>,
        created_at: Option<String>,
        conversation_id_str: Option<String>,
        id_str: Option<String>,
    }
    #[derive(Deserialize)]
    struct LegacyUser {
        name: Option<String>,
        screen_name: Option<String>,
    }

    let mut out = Vec::new();
    // Collect every object that has a `legacy` tweet block + a `core` user.
    fn walk<'a>(v: &'a serde_json::Value, acc: &mut Vec<&'a serde_json::Value>) {
        match v {
            serde_json::Value::Object(m) => {
                if m.contains_key("legacy") && m.contains_key("rest_id") {
                    acc.push(v);
                }
                for vv in m.values() {
                    walk(vv, acc);
                }
            }
            serde_json::Value::Array(a) => {
                for vv in a {
                    walk(vv, acc);
                }
            }
            _ => {}
        }
    }
    let mut results = Vec::new();
    walk(payload, &mut results);

    for r in results {
        let rest_id = r
            .get("rest_id")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string();
        if rest_id.is_empty() {
            continue;
        }
        let legacy: LegacyTweet = match serde_json::from_value(r["legacy"].clone()) {
            Ok(l) => l,
            Err(_) => continue,
        };
        let user = r
            .get("core")
            .and_then(|c| c.pointer("/user_results/result/legacy"))
            .cloned()
            .and_then(|u| serde_json::from_value::<LegacyUser>(u).ok())
            .unwrap_or(LegacyUser {
                name: None,
                screen_name: None,
            });
        let text = legacy.full_text.unwrap_or_default();
        if text.is_empty() {
            continue;
        }
        out.push(Tweet {
            rest_id: legacy.id_str.unwrap_or(rest_id.clone()),
            conversation_id: legacy.conversation_id_str.unwrap_or(rest_id),
            author_name: user.name.unwrap_or_else(|| "(unknown)".into()),
            author_handle: user.screen_name.unwrap_or_default(),
            author_id: author_id.to_string(),
            text,
            created_at_ms: legacy
                .created_at
                .map(|s| twitter_time_to_ms(&s))
                .unwrap_or(0),
        });
    }
    out
}

/// Parse `inbox_initial_state.json` → `Vec<TwitterDm>`. The response carries
/// `inbox_initial_state.entries[].message.message_data` + a `users` map.
/// Tolerant of unknown shapes. REQUIRES LIVE OPERATOR VALIDATION for the
/// exact entry nesting on large inboxes.
fn parse_dm_inbox(payload: &serde_json::Value) -> Vec<TwitterDm> {
    let state = payload
        .get("inbox_initial_state")
        .unwrap_or(payload);
    let users = state.get("users").cloned().unwrap_or(serde_json::Value::Null);
    let lookup_user = |id: &str| -> (String, String) {
        users
            .get(id)
            .map(|u| {
                (
                    u.get("name").and_then(|x| x.as_str()).unwrap_or("").into(),
                    u.get("screen_name")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .into(),
                )
            })
            .unwrap_or_default()
    };

    let mut out = Vec::new();
    let entries = state
        .get("entries")
        .and_then(|e| e.as_array())
        .cloned()
        .unwrap_or_default();
    for e in entries {
        let Some(md) = e.pointer("/message/message_data") else {
            continue;
        };
        let event_id = e
            .pointer("/message/id")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string();
        let conversation_id = md
            .get("conversation_id")
            .and_then(|x| x.as_str())
            .or_else(|| {
                e.pointer("/message/conversation_id").and_then(|x| x.as_str())
            })
            .unwrap_or_default()
            .to_string();
        let sender_id = md
            .get("sender_id")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string();
        let text = md
            .pointer("/text")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string();
        if event_id.is_empty() || sender_id.is_empty() || text.is_empty() {
            continue;
        }
        let (name, handle) = lookup_user(&sender_id);
        let created_at_ms = md
            .get("time")
            .and_then(|x| x.as_str())
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);
        out.push(TwitterDm {
            event_id,
            conversation_id,
            sender_name: if name.is_empty() {
                "(unknown)".into()
            } else {
                name
            },
            sender_handle: handle,
            sender_id,
            text,
            created_at_ms,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencoding_escapes_json_punctuation() {
        assert_eq!(urlencoding("{\"a\":1}"), "%7B%22a%22%3A1%7D");
        assert_eq!(urlencoding("abcXYZ09-_.~"), "abcXYZ09-_.~");
    }

    #[test]
    fn query_id_env_override_wins() {
        std::env::set_var("AUGMENTAGENT_TWITTER_CREATE_TWEET_QUERY_ID", "OVERRIDE123");
        assert_eq!(
            TwitterClient::query_id_for("CREATE_TWEET", DEFAULT_CREATE_TWEET_QUERY_ID),
            "OVERRIDE123"
        );
        std::env::remove_var("AUGMENTAGENT_TWITTER_CREATE_TWEET_QUERY_ID");
        assert_eq!(
            TwitterClient::query_id_for("CREATE_TWEET", DEFAULT_CREATE_TWEET_QUERY_ID),
            DEFAULT_CREATE_TWEET_QUERY_ID
        );
    }

    #[test]
    fn client_transaction_id_is_nonempty_and_urlsafe() {
        let id = gen_client_transaction_id();
        assert!(!id.is_empty());
        assert!(id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn filter_since_drops_old_and_equal_ids() {
        let tweets = vec![
            Tweet {
                rest_id: "100".into(),
                conversation_id: "100".into(),
                author_name: "a".into(),
                author_handle: "a".into(),
                author_id: "1".into(),
                text: "old".into(),
                created_at_ms: 0,
            },
            Tweet {
                rest_id: "200".into(),
                conversation_id: "200".into(),
                author_name: "a".into(),
                author_handle: "a".into(),
                author_id: "1".into(),
                text: "new".into(),
                created_at_ms: 0,
            },
        ];
        let kept = filter_since(tweets, Some("150"));
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].rest_id, "200");
    }

    #[test]
    fn filter_since_none_keeps_all() {
        let tweets = vec![Tweet {
            rest_id: "1".into(),
            conversation_id: "1".into(),
            author_name: "a".into(),
            author_handle: "a".into(),
            author_id: "1".into(),
            text: "x".into(),
            created_at_ms: 0,
        }];
        assert_eq!(filter_since(tweets, None).len(), 1);
    }

    #[test]
    fn twitter_time_parses_legacy_format() {
        // Wed Oct 10 20:19:24 +0000 2018 = 1539202764 s
        let ms = twitter_time_to_ms("Wed Oct 10 20:19:24 +0000 2018");
        assert_eq!(ms, 1539202764 * 1000);
        assert_eq!(twitter_time_to_ms("garbage"), 0);
    }

    #[test]
    fn parse_user_tweets_extracts_from_nested_result() {
        let payload = serde_json::json!({
            "data": { "user": { "result": { "timeline_v2": { "timeline": {
                "instructions": [ { "type": "TimelineAddEntries", "entries": [
                    { "content": { "itemContent": { "tweet_results": { "result": {
                        "rest_id": "1700000000000000001",
                        "core": { "user_results": { "result": { "legacy": {
                            "name": "Jane Doe", "screen_name": "janedoe"
                        }}}},
                        "legacy": {
                            "full_text": "shipped a thing",
                            "created_at": "Wed Oct 10 20:19:24 +0000 2018",
                            "conversation_id_str": "1700000000000000000",
                            "id_str": "1700000000000000001"
                        }
                    }}}}}
                ]}]
            }}}}}
        });
        let tweets = parse_user_tweets(&payload, "55");
        assert_eq!(tweets.len(), 1);
        let t = &tweets[0];
        assert_eq!(t.rest_id, "1700000000000000001");
        assert_eq!(t.author_name, "Jane Doe");
        assert_eq!(t.author_handle, "janedoe");
        assert_eq!(t.author_id, "55");
        assert_eq!(t.text, "shipped a thing");
        assert!(t.created_at_ms > 0);
    }

    #[test]
    fn parse_dm_inbox_extracts_messages() {
        let payload = serde_json::json!({
            "inbox_initial_state": {
                "users": {
                    "55": { "name": "Jane Doe", "screen_name": "janedoe" }
                },
                "entries": [
                    { "message": {
                        "id": "1800000000000000001",
                        "conversation_id": "55-99",
                        "message_data": {
                            "sender_id": "55",
                            "conversation_id": "55-99",
                            "text": "hey got a sec?",
                            "time": "1776630000000"
                        }
                    }}
                ]
            }
        });
        let dms = parse_dm_inbox(&payload);
        assert_eq!(dms.len(), 1);
        let d = &dms[0];
        assert_eq!(d.event_id, "1800000000000000001");
        assert_eq!(d.conversation_id, "55-99");
        assert_eq!(d.sender_id, "55");
        assert_eq!(d.sender_name, "Jane Doe");
        assert_eq!(d.sender_handle, "janedoe");
        assert_eq!(d.text, "hey got a sec?");
        assert_eq!(d.created_at_ms, 1776630000000);
    }

    #[test]
    fn parse_dm_inbox_skips_incomplete_entries() {
        let payload = serde_json::json!({
            "inbox_initial_state": {
                "users": {},
                "entries": [
                    { "message": { "id": "1", "message_data": { "sender_id": "55" } } },
                    { "trust_conversation": { "conversation_id": "x" } }
                ]
            }
        });
        assert!(parse_dm_inbox(&payload).is_empty());
    }

    #[test]
    fn find_string_field_recurses() {
        let v = serde_json::json!({ "a": { "b": { "rest_id": "999" } } });
        assert_eq!(find_string_field(&v, "rest_id").as_deref(), Some("999"));
    }
}
