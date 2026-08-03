//! `reqwest`-backed client for the SocialAPI.ai REST API.
//!
//! Every request carries `Authorization: Bearer <api_key>`. The base URL
//! defaults to [`DEFAULT_BASE_URL`] but is overridable per-client so the
//! wiremock tests (and a local proxy) can point it elsewhere.
//!
//! ## Response envelope (#543)
//!
//! The live API wraps every response in `{"data": ..., "pagination"/"count":
//! ...}` and returns `data: null` (not `[]`) for an empty collection. List
//! helpers here unwrap that via [`Envelope`], tolerating a bare array too so
//! a mock or older deployment can't break decoding. Write endpoints return
//! the created object either enveloped or bare; [`extract_id`] pulls the id
//! from both.

use std::time::Duration;

use thiserror::Error;
use tracing::debug;

use crate::auth::SocialApiAuth;
use crate::types::{
    Account, Comment, CommentReplyRequest, ConnectResponse, Conversation, CreatePostRequest,
    CreatePostResponse, DmMessage, DmSendRequest, Envelope, InboxPost,
};

/// Production base URL. Always carries the trailing slash so relative joins
/// behave under [`reqwest::Url::join`] semantics (we build paths by hand here,
/// but keep the invariant for clarity).
pub const DEFAULT_BASE_URL: &str = "https://api.social-api.ai/v1/";

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("socialapi {status}: {body}")]
    Api {
        status: reqwest::StatusCode,
        body: String,
    },
    #[error("socialapi decode: {0}")]
    Decode(#[from] serde_json::Error),
}

/// Pull the created-object id out of a write response, whether the API
/// enveloped it (`{"data":{"id":...}}`) or returned it bare (`{"id":...}`).
/// Missing id → empty string; callers treat the id as advisory.
fn extract_id(v: &serde_json::Value) -> String {
    v.pointer("/data/id")
        .or_else(|| v.get("id"))
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string()
}

/// Unwrap a list response: `{"data":[...]}` (data possibly `null`) or a bare
/// `[...]`.
fn unwrap_list<T: serde::de::DeserializeOwned>(
    v: serde_json::Value,
) -> Result<Vec<T>, serde_json::Error> {
    if v.is_array() {
        return serde_json::from_value(v);
    }
    let env: Envelope<Vec<T>> = serde_json::from_value(v)?;
    Ok(env.data.unwrap_or_default())
}

/// Thin SocialAPI.ai REST client. Cheap to clone (wraps a `reqwest::Client`,
/// which is itself an `Arc` internally).
#[derive(Debug, Clone)]
pub struct SocialApiClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl SocialApiClient {
    /// Build a client from loaded [`SocialApiAuth`], pointing at production.
    pub fn new(auth: SocialApiAuth) -> Self {
        Self::with_base_url(auth, DEFAULT_BASE_URL)
    }

    /// Build a client with an explicit base URL (tests / local proxy). A
    /// trailing slash is normalised away so path joins are uniform.
    pub fn with_base_url(auth: SocialApiAuth, base_url: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client");
        Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: auth.api_key,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{}", self.base_url, path.trim_start_matches('/'))
    }

    /// Send a GET and return the raw JSON value, mapping non-2xx to
    /// [`ClientError::Api`].
    async fn get_value(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<serde_json::Value, ClientError> {
        let resp = self
            .http
            .get(self.url(path))
            .bearer_auth(&self.api_key)
            .query(query)
            .send()
            .await?;
        Self::parse(resp).await
    }

    /// Send a GET and unwrap a `{"data":[...]}` list envelope (bare arrays and
    /// `data: null` tolerated).
    async fn get_list<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<Vec<T>, ClientError> {
        Ok(unwrap_list(self.get_value(path, query).await?)?)
    }

    /// Send a POST with a JSON body and return the raw JSON response.
    async fn post_value<B: serde::Serialize>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<serde_json::Value, ClientError> {
        let resp = self
            .http
            .post(self.url(path))
            .bearer_auth(&self.api_key)
            .json(body)
            .send()
            .await?;
        Self::parse(resp).await
    }

    async fn parse<T: serde::de::DeserializeOwned>(
        resp: reqwest::Response,
    ) -> Result<T, ClientError> {
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ClientError::Api { status, body });
        }
        Ok(resp.json::<T>().await?)
    }

    /// Deserialize `v` as `T`, unwrapping a `{"data":{...}}` envelope first if
    /// one is present.
    fn from_enveloped<T: serde::de::DeserializeOwned>(
        v: serde_json::Value,
    ) -> Result<T, serde_json::Error> {
        match v {
            serde_json::Value::Object(ref m) if m.get("data").map_or(false, |d| d.is_object()) => {
                serde_json::from_value(m.get("data").cloned().unwrap_or_default())
            }
            other => serde_json::from_value(other),
        }
    }

    // --- accounts --------------------------------------------------------

    /// `POST /accounts/connect` — start connecting `platform`. OAuth platforms
    /// return an `auth_url`; credential platforms return an `account_id`.
    pub async fn connect_account(&self, platform: &str) -> anyhow::Result<ConnectResponse> {
        debug!(platform, "socialapi connect_account");
        let body = serde_json::json!({ "platform": platform });
        let v = self.post_value("accounts/connect", &body).await?;
        Ok(Self::from_enveloped(v)?)
    }

    /// `GET /accounts` — list connected accounts.
    pub async fn list_accounts(&self) -> anyhow::Result<Vec<Account>> {
        Ok(self.get_list("accounts", &[]).await?)
    }

    // --- posting ---------------------------------------------------------

    /// `POST /posts` — create (fan-out) a post across the request's targets.
    /// The returned `status` is the *initial* state (`publishing`); terminal
    /// states arrive via webhooks.
    pub async fn create_post(
        &self,
        req: &CreatePostRequest,
    ) -> anyhow::Result<CreatePostResponse> {
        debug!(targets = req.targets.len(), "socialapi create_post");
        let v = self.post_value("posts", req).await?;
        Ok(Self::from_enveloped(v)?)
    }

    // --- inbox -----------------------------------------------------------

    /// `GET /inbox/comments` — despite the path, this lists our own published
    /// POSTS (caption, permalink, `comment_count`), optionally scoped to one
    /// account. The actual comments live per-post behind
    /// [`list_comments`](Self::list_comments) (#543).
    pub async fn list_inbox_posts(
        &self,
        account_id: Option<&str>,
    ) -> anyhow::Result<Vec<InboxPost>> {
        let query: Vec<(&str, &str)> = account_id
            .map(|a| vec![("account_id", a)])
            .unwrap_or_default();
        Ok(self.get_list("inbox/comments", &query).await?)
    }

    /// `GET /inbox/comments/{post_id}?account_id=...` — list one post's
    /// comments. The live API hard-requires `account_id` as a query param
    /// (400 `validation.field_required` without it); pass the post's owning
    /// account from [`InboxPost::account_id`].
    pub async fn list_comments(
        &self,
        post_id: &str,
        account_id: Option<&str>,
    ) -> anyhow::Result<Vec<Comment>> {
        let query: Vec<(&str, &str)> = account_id
            .map(|a| vec![("account_id", a)])
            .unwrap_or_default();
        Ok(self
            .get_list(&format!("inbox/comments/{post_id}"), &query)
            .await?)
    }

    /// `GET /inbox/conversations` — list DM threads (newest activity first),
    /// optionally scoped to one account. First page only.
    pub async fn list_conversations(
        &self,
        account_id: Option<&str>,
    ) -> anyhow::Result<Vec<Conversation>> {
        let query: Vec<(&str, &str)> = account_id
            .map(|a| vec![("account_id", a)])
            .unwrap_or_default();
        Ok(self.get_list("inbox/conversations", &query).await?)
    }

    /// `GET /inbox/conversations/{conversation_id}/messages` — list the
    /// messages of one DM thread, **newest first**. Conversations from
    /// [`list_conversations`](Self::list_conversations) carry no messages;
    /// this is the only way to read them (#543).
    pub async fn list_messages(
        &self,
        conversation_id: &str,
    ) -> anyhow::Result<Vec<DmMessage>> {
        Ok(self
            .get_list(
                &format!("inbox/conversations/{conversation_id}/messages"),
                &[],
            )
            .await?)
    }

    /// `POST /inbox/comments/{post_id}` — reply to a comment thread. Returns
    /// the created reply's id (may be empty if the API omits it). Consumes an
    /// interaction credit. `account_id` is sent both in the body and as a
    /// query param — the sibling GET hard-requires the query form.
    pub async fn reply_comment(
        &self,
        post_id: &str,
        req: &CommentReplyRequest,
    ) -> anyhow::Result<String> {
        debug!(post_id, "socialapi reply_comment");
        let path = match req.account_id.as_deref() {
            Some(a) => format!("inbox/comments/{post_id}?account_id={a}"),
            None => format!("inbox/comments/{post_id}"),
        };
        let v = self.post_value(&path, req).await?;
        Ok(extract_id(&v))
    }

    /// `POST /inbox/conversations/{conversation_id}/messages` — send a reply
    /// into an existing DM thread (#244). Returns the created message's id
    /// (may be empty if the API omits it). The path targets the *messages*
    /// collection — POSTing the bare conversation is not a send (#543).
    pub async fn send_dm(
        &self,
        conversation_id: &str,
        req: &DmSendRequest,
    ) -> anyhow::Result<String> {
        debug!(conversation_id, "socialapi send_dm");
        let v = self
            .post_value(
                &format!("inbox/conversations/{conversation_id}/messages"),
                req,
            )
            .await?;
        Ok(extract_id(&v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::PostTarget;
    use std::sync::{Arc, Mutex};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    fn client(server: &MockServer) -> SocialApiClient {
        SocialApiClient::with_base_url(SocialApiAuth::new("sk_test_key"), server.uri())
    }

    /// create_post: assert the bearer header and the exact JSON request body
    /// (`text`, `media_ids`, `publish_now` — the real wire fields, #543).
    #[tokio::test]
    async fn create_post_sends_bearer_and_real_wire_fields() {
        let server = MockServer::start().await;
        let captured: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
        let sink = captured.clone();

        Mock::given(method("POST"))
            .and(path("/posts"))
            .and(header("authorization", "Bearer sk_test_key"))
            .respond_with(move |req: &Request| {
                *sink.lock().unwrap() = Some(serde_json::from_slice(&req.body).unwrap());
                ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({ "data": { "id": "post_42", "status": "publishing" } }),
                )
            })
            .mount(&server)
            .await;

        let req = CreatePostRequest {
            text: "hello world".into(),
            targets: vec![PostTarget {
                account_id: "acc_1".into(),
                platform: "twitter".into(),
            }],
            media_ids: Some(vec!["m_1".into()]),
            publish_now: Some(true),
            scheduled_at: None,
        };
        let resp = client(&server).create_post(&req).await.unwrap();
        assert_eq!(resp.id, "post_42");
        assert_eq!(resp.status, "publishing");

        let body = captured.lock().unwrap().clone().expect("body captured");
        assert_eq!(body["text"], "hello world");
        assert!(body.get("body").is_none(), "old `body` field must not be sent");
        assert_eq!(body["targets"][0]["account_id"], "acc_1");
        assert_eq!(body["targets"][0]["platform"], "twitter");
        assert_eq!(body["media_ids"][0], "m_1");
        assert_eq!(body["publish_now"], true);
    }

    /// A bare (un-enveloped) create response still parses.
    #[tokio::test]
    async fn create_post_tolerates_bare_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/posts"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "id": "p1", "status": "published" })),
            )
            .mount(&server)
            .await;
        let req = CreatePostRequest {
            text: "x".into(),
            targets: vec![],
            media_ids: None,
            publish_now: Some(true),
            scheduled_at: None,
        };
        let resp = client(&server).create_post(&req).await.unwrap();
        assert_eq!(resp.id, "p1");
    }

    /// reply_comment: assert the bearer header, the `{post_id}` in the path,
    /// the `account_id` query param (the sibling GET hard-requires it), and
    /// the JSON reply body; the created id comes back from the envelope.
    #[tokio::test]
    async fn reply_comment_sends_bearer_path_query_and_body() {
        let server = MockServer::start().await;
        let captured: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
        let sink = captured.clone();

        Mock::given(method("POST"))
            .and(path("/inbox/comments/post_7"))
            .and(wiremock::matchers::query_param("account_id", "acc_1"))
            .and(header("authorization", "Bearer sk_test_key"))
            .respond_with(move |req: &Request| {
                *sink.lock().unwrap() = Some(serde_json::from_slice(&req.body).unwrap());
                ResponseTemplate::new(201)
                    .set_body_json(serde_json::json!({ "data": { "id": "cmt_8" } }))
            })
            .mount(&server)
            .await;

        let req = CommentReplyRequest {
            text: "thanks!".into(),
            comment_id: Some("cmt_7".into()),
            private: None,
            account_id: Some("acc_1".into()),
        };
        let id = client(&server).reply_comment("post_7", &req).await.unwrap();
        assert_eq!(id, "cmt_8");

        let body = captured.lock().unwrap().clone().expect("body captured");
        assert_eq!(
            body,
            serde_json::json!({
                "text": "thanks!",
                "comment_id": "cmt_7",
                "account_id": "acc_1"
            })
        );
    }

    /// send_dm: the send targets the conversation's MESSAGES collection
    /// (#543) and carries the sending account when known.
    #[tokio::test]
    async fn send_dm_posts_to_messages_collection() {
        let server = MockServer::start().await;
        let captured: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
        let sink = captured.clone();

        Mock::given(method("POST"))
            .and(path("/inbox/conversations/conv_3/messages"))
            .and(header("authorization", "Bearer sk_test_key"))
            .respond_with(move |req: &Request| {
                *sink.lock().unwrap() = Some(serde_json::from_slice(&req.body).unwrap());
                ResponseTemplate::new(201)
                    .set_body_json(serde_json::json!({ "data": { "id": "dm_9" } }))
            })
            .mount(&server)
            .await;

        let req = DmSendRequest {
            text: "on it!".into(),
            account_id: Some("acc_1".into()),
            attachment_url: None,
        };
        let id = client(&server).send_dm("conv_3", &req).await.unwrap();
        assert_eq!(id, "dm_9");

        let body = captured.lock().unwrap().clone().expect("body captured");
        assert_eq!(
            body,
            serde_json::json!({ "text": "on it!", "account_id": "acc_1" })
        );
    }

    /// Accounts come back in the live `{"data":[...],"count":n}` envelope
    /// with `name`/`username` fields.
    #[tokio::test]
    async fn list_accounts_parses_enveloped_live_shape() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/accounts"))
            .and(header("authorization", "Bearer sk_test_key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    {
                        "id": "a1",
                        "brand_id": "b1",
                        "platform": "instagram",
                        "name": "My Brand",
                        "username": "mybrand",
                        "status": "active"
                    }
                ],
                "count": 1
            })))
            .mount(&server)
            .await;

        let accts = client(&server).list_accounts().await.unwrap();
        assert_eq!(accts.len(), 1);
        assert_eq!(accts[0].platform, "instagram");
        assert_eq!(accts[0].username, "mybrand");
        assert_eq!(accts[0].name, "My Brand");
    }

    /// A bare array (mock/legacy) still parses.
    #[tokio::test]
    async fn list_accounts_tolerates_bare_array() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/accounts"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                { "id": "a1", "platform": "linkedin", "name": "N", "username": "n" }
            ])))
            .mount(&server)
            .await;
        let accts = client(&server).list_accounts().await.unwrap();
        assert_eq!(accts.len(), 1);
    }

    #[tokio::test]
    async fn connect_account_sends_platform_and_returns_url() {
        let server = MockServer::start().await;
        let captured: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
        let sink = captured.clone();
        Mock::given(method("POST"))
            .and(path("/accounts/connect"))
            .respond_with(move |req: &Request| {
                *sink.lock().unwrap() = Some(serde_json::from_slice(&req.body).unwrap());
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "auth_url": "https://oauth.example/go" }))
            })
            .mount(&server)
            .await;

        let resp = client(&server).connect_account("linkedin").await.unwrap();
        assert_eq!(resp.auth_url.as_deref(), Some("https://oauth.example/go"));
        let body = captured.lock().unwrap().clone().unwrap();
        assert_eq!(body["platform"], "linkedin");
    }

    #[tokio::test]
    async fn non_success_maps_to_api_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/accounts"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad key"))
            .mount(&server)
            .await;

        let err = client(&server).list_accounts().await.unwrap_err();
        let ce = err.downcast::<ClientError>().unwrap();
        match ce {
            ClientError::Api { status, body } => {
                assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED);
                assert_eq!(body, "bad key");
            }
            other => panic!("expected Api error, got {other:?}"),
        }
    }

    /// The live API returns `{"data":null}` — not `[]` — for an empty inbox.
    /// This exact response produced "error decoding response body" in
    /// production before #543.
    #[tokio::test]
    async fn list_inbox_posts_tolerates_null_data() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/inbox/comments"))
            .and(wiremock::matchers::query_param("account_id", "acc_9"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "data": null, "pagination": { "has_more": false } }),
            ))
            .mount(&server)
            .await;

        let out = client(&server).list_inbox_posts(Some("acc_9")).await.unwrap();
        assert!(out.is_empty());
    }

    /// Per-post comments: path carries the post id, query the owning account.
    #[tokio::test]
    async fn list_comments_hits_per_post_path_with_account_query() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/inbox/comments/post_1"))
            .and(wiremock::matchers::query_param("account_id", "acc_1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{
                    "platform_id": "cmt_1", "platform": "instagram", "text": "🔥",
                    "author_name": "jane", "author_username": "jane", "is_owner": false,
                    "created_at": "2026-08-01T13:57:41Z",
                    "capabilities": {"can_reply": true}
                }],
                "pagination": {"has_more": false, "next_cursor": ""}
            })))
            .mount(&server)
            .await;

        let out = client(&server)
            .list_comments("post_1", Some("acc_1"))
            .await
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].platform_id, "cmt_1");
        assert!(!out[0].is_owner);
    }

    /// Conversations + messages: the live two-endpoint flow round-trips.
    #[tokio::test]
    async fn conversations_then_messages_parse_live_shapes() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/inbox/conversations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{
                    "id": "conv_1",
                    "account_id": "acc_1",
                    "platform": "instagram",
                    "participant_id": "451",
                    "participant_name": "maehavingfun",
                    "last_message": "see you there",
                    "last_message_at": "2026-08-03T00:39:29Z",
                    "status": "active",
                    "unread_count": 0
                }],
                "pagination": { "has_more": false }
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/inbox/conversations/conv_1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    {
                        "id": "m2", "conversation_id": "conv_1", "direction": "outgoing",
                        "text": "see you there", "sender_id": "178", "sender_name": "me",
                        "created_at": "2026-08-03T00:39:29Z"
                    },
                    {
                        "id": "m1", "conversation_id": "conv_1", "direction": "incoming",
                        "text": "friday works!", "sender_id": "451", "sender_name": "maehavingfun",
                        "created_at": "2026-08-02T21:38:04Z"
                    }
                ]
            })))
            .mount(&server)
            .await;

        let c = client(&server);
        let convs = c.list_conversations(None).await.unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].participant_name, "maehavingfun");
        let msgs = c.list_messages("conv_1").await.unwrap();
        assert_eq!(msgs.len(), 2);
        assert!(msgs[0].is_outgoing());
        assert!(msgs[1].is_incoming());
    }
}
