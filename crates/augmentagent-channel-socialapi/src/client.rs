//! `reqwest`-backed client for the SocialAPI.ai REST API.
//!
//! Every request carries `Authorization: Bearer <api_key>`. The base URL
//! defaults to [`DEFAULT_BASE_URL`] but is overridable per-client so the
//! wiremock tests (and a local proxy) can point it elsewhere.

use std::time::Duration;

use thiserror::Error;
use tracing::debug;

use crate::auth::SocialApiAuth;
use crate::types::{
    Account, Comment, ConnectResponse, Conversation, CreatePostRequest, CreatePostResponse,
    DmMessage, MediaUploadRequest, MediaUploadResponse, ReplyRequest,
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

    /// Send a GET and deserialize the JSON body, mapping non-2xx to
    /// [`ClientError::Api`].
    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T, ClientError> {
        let resp = self
            .http
            .get(self.url(path))
            .bearer_auth(&self.api_key)
            .query(query)
            .send()
            .await?;
        Self::parse(resp).await
    }

    /// Send a POST with a JSON body and deserialize the JSON response.
    async fn post_json<B: serde::Serialize, T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, ClientError> {
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

    // --- accounts --------------------------------------------------------

    /// `POST /accounts/connect` — start the OAuth flow for `platform`,
    /// returning a URL to redirect the user to.
    pub async fn connect_account(&self, platform: &str) -> anyhow::Result<ConnectResponse> {
        debug!(platform, "socialapi connect_account");
        let body = serde_json::json!({ "platform": platform });
        Ok(self.post_json("accounts/connect", &body).await?)
    }

    /// `GET /accounts` — list connected accounts.
    pub async fn list_accounts(&self) -> anyhow::Result<Vec<Account>> {
        Ok(self.get_json("accounts", &[]).await?)
    }

    // --- posting ---------------------------------------------------------

    /// `POST /posts` — create (fan-out) a post across the request's targets.
    pub async fn create_post(
        &self,
        req: &CreatePostRequest,
    ) -> anyhow::Result<CreatePostResponse> {
        debug!(targets = req.targets.len(), "socialapi create_post");
        Ok(self.post_json("posts", req).await?)
    }

    // --- inbox -----------------------------------------------------------

    /// `GET /inbox/comments` — list comments, optionally scoped to one
    /// account.
    pub async fn list_comments(
        &self,
        account_id: Option<&str>,
    ) -> anyhow::Result<Vec<Comment>> {
        let query: Vec<(&str, &str)> = account_id
            .map(|a| vec![("account_id", a)])
            .unwrap_or_default();
        Ok(self.get_json("inbox/comments", &query).await?)
    }

    /// `GET /inbox/conversations` — list DM threads, optionally scoped to one
    /// account.
    pub async fn list_conversations(
        &self,
        account_id: Option<&str>,
    ) -> anyhow::Result<Vec<Conversation>> {
        let query: Vec<(&str, &str)> = account_id
            .map(|a| vec![("account_id", a)])
            .unwrap_or_default();
        Ok(self.get_json("inbox/conversations", &query).await?)
    }

    /// `POST /inbox/comments/{post_id}` — reply to a comment thread.
    pub async fn reply_comment(
        &self,
        post_id: &str,
        req: &ReplyRequest,
    ) -> anyhow::Result<Comment> {
        debug!(post_id, "socialapi reply_comment");
        Ok(self
            .post_json(&format!("inbox/comments/{post_id}"), req)
            .await?)
    }

    /// `POST /inbox/conversations/{conversation_id}` — send a reply into an
    /// existing DM thread. Mirrors [`reply_comment`](Self::reply_comment) but
    /// targets a conversation and returns the created [`DmMessage`]. Used by
    /// the approve→send path for `kind = "dm"` (#244).
    pub async fn send_dm(
        &self,
        conversation_id: &str,
        req: &ReplyRequest,
    ) -> anyhow::Result<DmMessage> {
        debug!(conversation_id, "socialapi send_dm");
        Ok(self
            .post_json(&format!("inbox/conversations/{conversation_id}"), req)
            .await?)
    }

    // --- media -----------------------------------------------------------

    /// `POST /media/upload-url` — request a presigned upload slot.
    pub async fn media_upload_url(
        &self,
        req: &MediaUploadRequest,
    ) -> anyhow::Result<MediaUploadResponse> {
        Ok(self.post_json("media/upload-url", req).await?)
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

    /// create_post: assert the bearer header and the exact JSON request body.
    #[tokio::test]
    async fn create_post_sends_bearer_and_body() {
        let server = MockServer::start().await;
        let captured: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
        let sink = captured.clone();

        Mock::given(method("POST"))
            .and(path("/posts"))
            .and(header("authorization", "Bearer sk_test_key"))
            .respond_with(move |req: &Request| {
                *sink.lock().unwrap() = Some(serde_json::from_slice(&req.body).unwrap());
                ResponseTemplate::new(200).set_body_json(serde_json::json!({ "id": "post_42" }))
            })
            .mount(&server)
            .await;

        let req = CreatePostRequest {
            targets: vec![PostTarget {
                account_id: "acc_1".into(),
                platform: "twitter".into(),
            }],
            body: "hello world".into(),
            media: Some(vec!["m_1".into()]),
        };
        let resp = client(&server).create_post(&req).await.unwrap();
        assert_eq!(resp.id, "post_42");

        let body = captured.lock().unwrap().clone().expect("body captured");
        assert_eq!(body["body"], "hello world");
        assert_eq!(body["targets"][0]["account_id"], "acc_1");
        assert_eq!(body["targets"][0]["platform"], "twitter");
        assert_eq!(body["media"][0], "m_1");
    }

    /// reply_comment: assert the bearer header, the `{post_id}` in the path,
    /// and the JSON reply body.
    #[tokio::test]
    async fn reply_comment_sends_bearer_path_and_body() {
        let server = MockServer::start().await;
        let captured: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
        let sink = captured.clone();

        Mock::given(method("POST"))
            .and(path("/inbox/comments/cmt_7"))
            .and(header("authorization", "Bearer sk_test_key"))
            .respond_with(move |req: &Request| {
                *sink.lock().unwrap() = Some(serde_json::from_slice(&req.body).unwrap());
                ResponseTemplate::new(201).set_body_json(serde_json::json!({
                    "id": "cmt_8",
                    "post_id": "cmt_7",
                    "author": "me",
                    "text": "thanks!",
                    "created_at": "2026-05-28T00:00:00Z"
                }))
            })
            .mount(&server)
            .await;

        let req = ReplyRequest {
            text: "thanks!".into(),
        };
        let resp = client(&server).reply_comment("cmt_7", &req).await.unwrap();
        assert_eq!(resp.id, "cmt_8");
        assert_eq!(resp.text, "thanks!");

        let body = captured.lock().unwrap().clone().expect("body captured");
        assert_eq!(body, serde_json::json!({ "text": "thanks!" }));
    }

    /// send_dm: assert the bearer header, the `{conversation_id}` in the path,
    /// the JSON reply body, and that the response parses into a `DmMessage`.
    #[tokio::test]
    async fn send_dm_sends_bearer_path_and_body() {
        let server = MockServer::start().await;
        let captured: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
        let sink = captured.clone();

        Mock::given(method("POST"))
            .and(path("/inbox/conversations/conv_3"))
            .and(header("authorization", "Bearer sk_test_key"))
            .respond_with(move |req: &Request| {
                *sink.lock().unwrap() = Some(serde_json::from_slice(&req.body).unwrap());
                ResponseTemplate::new(201).set_body_json(serde_json::json!({
                    "id": "dm_9",
                    "author": "me",
                    "text": "on it!",
                    "created_at": "2026-05-28T00:00:00Z"
                }))
            })
            .mount(&server)
            .await;

        let req = ReplyRequest {
            text: "on it!".into(),
        };
        let resp = client(&server).send_dm("conv_3", &req).await.unwrap();
        assert_eq!(resp.id, "dm_9");
        assert_eq!(resp.text, "on it!");

        let body = captured.lock().unwrap().clone().expect("body captured");
        assert_eq!(body, serde_json::json!({ "text": "on it!" }));
    }

    #[tokio::test]
    async fn list_accounts_parses_array() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/accounts"))
            .and(header("authorization", "Bearer sk_test_key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {
                    "id": "a1",
                    "brand_id": "b1",
                    "platform": "instagram",
                    "display_name": "My Brand",
                    "handle": "mybrand"
                }
            ])))
            .mount(&server)
            .await;

        let accts = client(&server).list_accounts().await.unwrap();
        assert_eq!(accts.len(), 1);
        assert_eq!(accts[0].platform, "instagram");
        assert_eq!(accts[0].handle, "mybrand");
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
        assert_eq!(resp.auth_url, "https://oauth.example/go");
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

    #[tokio::test]
    async fn list_comments_scopes_by_account_query() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/inbox/comments"))
            .and(wiremock::matchers::query_param("account_id", "acc_9"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&server)
            .await;

        let out = client(&server).list_comments(Some("acc_9")).await.unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn media_upload_url_round_trips() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/media/upload-url"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "media_id": "m_1",
                "upload_url": "https://upload.example/put"
            })))
            .mount(&server)
            .await;

        let resp = client(&server)
            .media_upload_url(&MediaUploadRequest {
                content_type: "image/png".into(),
                size_bytes: Some(10),
            })
            .await
            .unwrap();
        assert_eq!(resp.media_id, "m_1");
        assert_eq!(resp.upload_url, "https://upload.example/put");
    }
}
