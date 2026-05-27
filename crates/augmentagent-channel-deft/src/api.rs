//! Deftform REST client — **gated**.
//!
//! Narrow scope for the C&C spike:
//!   1. `GET /workspace` — validate a freshly-pasted token (whoami analogue).
//!   2. `GET /forms` — discover form ids.
//!   3. `GET /responses/{formId}` — the poll primitive (new submissions).
//!
//! Every network method first checks [`crate::deft_enabled`] and returns
//! [`DeftError::Disabled`] when the arming gate is unset, so the crate is
//! provably inert in production until armed. The endpoints, headers, error
//! envelope and rate limits implement `docs/deft-protocol.md` §2–§3.
//!
//! `GET /responses/{formId}` decoding is **defensive** because the public
//! docs do not pin the envelope (`REQUIRES LIVE VALIDATION`): we accept
//! either a bare `[ ... ]` array, a `{ "data": [ ... ] }` wrapper, or a
//! `{ "data": { "responses": [ ... ] } }` wrapper, ignoring unknown keys.

use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use serde::Deserialize;
use thiserror::Error;

use crate::auth::DeftAuth;
use crate::deft_enabled;
use crate::types::{DeftForm, DeftSubmission};

#[derive(Debug, Error)]
pub enum DeftError {
    #[error("deft channel disabled (set AUGMENTAGENT_DEFT_ENABLED + persist a workspace token)")]
    Disabled,
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("auth invalid (401/403); rotate token via `augmentagent deft login`")]
    AuthInvalid,
    #[error("rate limited; remaining={remaining:?}")]
    RateLimited { remaining: Option<i64> },
    #[error("deft {status}: {message}")]
    Status { status: u16, message: String },
    #[error("decode: {0}")]
    Decode(String),
    #[error("config: {0}")]
    Config(String),
}

/// Trait so the channel can be exercised against a stub in unit tests.
#[async_trait]
pub trait DeftApi: Send + Sync {
    /// `GET /workspace` — returns the workspace id on success (token probe).
    async fn whoami(&self) -> Result<String, DeftError>;

    /// `GET /responses/{form_id}` — all submissions for a form. Dedup is the
    /// caller's job (by `DeftSubmission::dedup_id`).
    async fn list_responses(&self, form_id: &str) -> Result<Vec<DeftSubmission>, DeftError>;

    /// `GET /forms` — every form in the workspace. Used by the channel for
    /// auto-discovery (#157) so the operator does not have to maintain an
    /// explicit `form_ids` allowlist. Default impl returns `Ok(vec![])` so
    /// existing test stubs (e.g. the one in `channel.rs`) don't need updating
    /// — only real clients need to implement this.
    async fn list_forms(&self) -> Result<Vec<DeftForm>, DeftError> {
        Ok(Vec::new())
    }
}

/// Real REST client. Inert unless [`deft_enabled`] is true.
pub struct DeftClient {
    http: reqwest::Client,
    auth: DeftAuth,
}

impl DeftClient {
    pub fn new(auth: DeftAuth) -> Result<Self, DeftError> {
        let http = reqwest::Client::builder()
            .build()
            .map_err(DeftError::Http)?;
        Ok(Self { http, auth })
    }

    fn base(&self) -> &str {
        self.auth.base_url.trim_end_matches('/')
    }

    fn headers(&self) -> Result<HeaderMap, DeftError> {
        let mut h = HeaderMap::new();
        let bearer = format!("Bearer {}", self.auth.token);
        h.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&bearer).map_err(|e| DeftError::Config(e.to_string()))?,
        );
        h.insert(ACCEPT, HeaderValue::from_static("application/json"));
        h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        Ok(h)
    }

    /// Map status + rate-limit headers onto our error taxonomy, then decode.
    async fn check<T: for<'de> Deserialize<'de>>(
        &self,
        resp: reqwest::Response,
    ) -> Result<T, DeftError> {
        let status = resp.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(DeftError::AuthInvalid);
        }
        if status.as_u16() == 429 {
            let remaining = resp
                .headers()
                .get("x-ratelimit-remaining")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<i64>().ok());
            return Err(DeftError::RateLimited { remaining });
        }
        if !status.is_success() {
            // Vendor error envelope: { "success": false, "message": "..." }
            let body = resp.text().await.unwrap_or_default();
            let message = serde_json::from_str::<ErrEnvelope>(&body)
                .map(|e| e.message)
                .unwrap_or(body);
            return Err(DeftError::Status {
                status: status.as_u16(),
                message,
            });
        }
        let text = resp
            .text()
            .await
            .map_err(|e| DeftError::Decode(e.to_string()))?;
        serde_json::from_str::<T>(&text).map_err(|e| DeftError::Decode(e.to_string()))
    }
}

#[derive(Deserialize)]
struct ErrEnvelope {
    #[serde(default)]
    message: String,
}

/// Defensive decode for `GET /responses/{formId}`. The public docs don't pin
/// the envelope (`REQUIRES LIVE VALIDATION`); accept the three plausible
/// shapes. TODO(#116): collapse to the real shape once captured live.
#[derive(Deserialize)]
#[serde(untagged)]
enum ResponsesEnvelope {
    Bare(Vec<DeftSubmission>),
    Wrapped { data: Vec<DeftSubmission> },
    DoubleWrapped { data: ResponsesInner },
}

#[derive(Deserialize)]
struct ResponsesInner {
    #[serde(default, alias = "responses", alias = "submissions")]
    items: Vec<DeftSubmission>,
}

impl ResponsesEnvelope {
    fn into_vec(self) -> Vec<DeftSubmission> {
        match self {
            ResponsesEnvelope::Bare(v) => v,
            ResponsesEnvelope::Wrapped { data } => data,
            ResponsesEnvelope::DoubleWrapped { data } => data.items,
        }
    }
}

#[derive(Deserialize)]
struct WorkspaceEnvelope {
    #[serde(default)]
    data: WorkspaceData,
}

#[derive(Deserialize, Default)]
struct WorkspaceData {
    #[serde(default, alias = "workspace_id")]
    id: String,
}

/// Defensive decode for `GET /forms`. Public docs do not pin the outer frame
/// (`REQUIRES LIVE VALIDATION`); accept the same three shapes as
/// `/responses/{formId}`: a bare array, a `{ "data": [ ... ] }` wrapper, or a
/// `{ "data": { "forms": [ ... ] } }` wrapper. Mirrors `ResponsesEnvelope`.
#[derive(Deserialize)]
#[serde(untagged)]
enum FormsEnvelope {
    Bare(Vec<DeftForm>),
    Wrapped { data: Vec<DeftForm> },
    DoubleWrapped { data: FormsInner },
}

#[derive(Deserialize)]
struct FormsInner {
    #[serde(default, alias = "forms", alias = "items")]
    items: Vec<DeftForm>,
}

impl FormsEnvelope {
    fn into_vec(self) -> Vec<DeftForm> {
        match self {
            FormsEnvelope::Bare(v) => v,
            FormsEnvelope::Wrapped { data } => data,
            FormsEnvelope::DoubleWrapped { data } => data.items,
        }
    }
}

#[async_trait]
impl DeftApi for DeftClient {
    async fn whoami(&self) -> Result<String, DeftError> {
        if !deft_enabled() {
            return Err(DeftError::Disabled);
        }
        let url = format!("{}/workspace", self.base());
        let resp = self.http.get(&url).headers(self.headers()?).send().await?;
        let ws: WorkspaceEnvelope = self.check(resp).await?;
        Ok(ws.data.id)
    }

    async fn list_responses(&self, form_id: &str) -> Result<Vec<DeftSubmission>, DeftError> {
        if !deft_enabled() {
            return Err(DeftError::Disabled);
        }
        let url = format!("{}/responses/{form_id}", self.base());
        let resp = self.http.get(&url).headers(self.headers()?).send().await?;
        let env: ResponsesEnvelope = self.check(resp).await?;
        let mut subs = env.into_vec();
        // The responses payload may not echo the form id per submission
        // (REQUIRES LIVE VALIDATION); stamp it so downstream routing works.
        for s in &mut subs {
            if s.form_id.is_empty() {
                s.form_id = form_id.to_string();
            }
        }
        Ok(subs)
    }

    async fn list_forms(&self) -> Result<Vec<DeftForm>, DeftError> {
        if !deft_enabled() {
            return Err(DeftError::Disabled);
        }
        let url = format!("{}/forms", self.base());
        let resp = self.http.get(&url).headers(self.headers()?).send().await?;
        let env: FormsEnvelope = self.check(resp).await?;
        Ok(env.into_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_auth(base: &str) -> DeftAuth {
        DeftAuth {
            workspace_id: "ws1".into(),
            token: "dft_test".into(),
            base_url: base.into(),
            fetched_at_ms: 0,
        }
    }

    #[tokio::test]
    async fn methods_are_inert_when_gate_off() {
        let _g = crate::gate_test_lock().await;
        crate::set_gate(false);
        let c = DeftClient::new(sample_auth("http://127.0.0.1:1")).unwrap();
        // No network is performed — the gate short-circuits first.
        assert!(matches!(c.whoami().await, Err(DeftError::Disabled)));
        assert!(matches!(
            c.list_responses("F1").await,
            Err(DeftError::Disabled)
        ));
        assert!(matches!(c.list_forms().await, Err(DeftError::Disabled)));
    }

    #[tokio::test]
    async fn list_responses_decodes_bare_array() {
        let _g = crate::gate_test_lock().await;
        crate::set_gate(true);
        let mut server = mockito::Server::new_async().await;
        let body = r#"[
            {"uuid":"u1","id":"s1","data":[
                {"label":"Command","response":"approve","uuid":"f1","custom_key":"agent_command"}
            ]}
        ]"#;
        let _m = server
            .mock("GET", "/responses/F1")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(body)
            .create_async()
            .await;
        let c = DeftClient::new(sample_auth(&server.url())).unwrap();
        let subs = c.list_responses("F1").await.unwrap();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].dedup_id(), "deft:u1");
        // form_id stamped from the route since the payload omitted it.
        assert_eq!(subs[0].form_id, "F1");
        crate::set_gate(false);
    }

    #[tokio::test]
    async fn list_responses_decodes_data_wrapper() {
        let _g = crate::gate_test_lock().await;
        crate::set_gate(true);
        let mut server = mockito::Server::new_async().await;
        let body = r#"{"success":true,"data":[{"uuid":"u2","data":[]}]}"#;
        let _m = server
            .mock("GET", "/responses/F2")
            .with_status(200)
            .with_body(body)
            .create_async()
            .await;
        let c = DeftClient::new(sample_auth(&server.url())).unwrap();
        let subs = c.list_responses("F2").await.unwrap();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].uuid, "u2");
        crate::set_gate(false);
    }

    #[tokio::test]
    async fn auth_invalid_surfaces_clean() {
        let _g = crate::gate_test_lock().await;
        crate::set_gate(true);
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("GET", "/responses/F1")
            .with_status(401)
            .with_body(r#"{"success":false,"message":"bad token"}"#)
            .create_async()
            .await;
        let c = DeftClient::new(sample_auth(&server.url())).unwrap();
        let err = c.list_responses("F1").await.unwrap_err();
        assert!(matches!(err, DeftError::AuthInvalid));
        crate::set_gate(false);
    }

    #[tokio::test]
    async fn rate_limit_surfaces_remaining() {
        let _g = crate::gate_test_lock().await;
        crate::set_gate(true);
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("GET", "/responses/F1")
            .with_status(429)
            .with_header("x-ratelimit-remaining", "0")
            .with_body("{}")
            .create_async()
            .await;
        let c = DeftClient::new(sample_auth(&server.url())).unwrap();
        match c.list_responses("F1").await.unwrap_err() {
            DeftError::RateLimited { remaining } => assert_eq!(remaining, Some(0)),
            other => panic!("expected RateLimited, got {other:?}"),
        }
        crate::set_gate(false);
    }

    #[tokio::test]
    async fn list_forms_happy_path_returns_forms() {
        let _g = crate::gate_test_lock().await;
        crate::set_gate(true);
        let mut server = mockito::Server::new_async().await;
        // Vendor envelope shape (matches `GET /workspace`):
        // `{ "success": true, "data": [ ... ] }`. The defensive decode also
        // accepts a bare array or a `data.forms[]` wrapper.
        let body = r#"{
            "success": true,
            "data": [
                {"id":"OUm6T9","name":"Contact form","created_at":"2026-05-01T10:00:00Z"},
                {"id":"AbCdEf","name":"Newsletter","created_at":"2026-05-10T12:00:00Z"}
            ]
        }"#;
        let _m = server
            .mock("GET", "/forms")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(body)
            .create_async()
            .await;
        let c = DeftClient::new(sample_auth(&server.url())).unwrap();
        let forms = c.list_forms().await.unwrap();
        assert_eq!(forms.len(), 2);
        assert_eq!(forms[0].id, "OUm6T9");
        assert_eq!(forms[0].name, "Contact form");
        assert_eq!(forms[0].created_at, "2026-05-01T10:00:00Z");
        assert_eq!(forms[1].id, "AbCdEf");
        assert_eq!(forms[1].name, "Newsletter");
        crate::set_gate(false);
    }

    #[tokio::test]
    async fn list_forms_auth_invalid_surfaces_clean() {
        let _g = crate::gate_test_lock().await;
        crate::set_gate(true);
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("GET", "/forms")
            .with_status(401)
            .with_body(r#"{"success":false,"message":"bad token"}"#)
            .create_async()
            .await;
        let c = DeftClient::new(sample_auth(&server.url())).unwrap();
        let err = c.list_forms().await.unwrap_err();
        assert!(matches!(err, DeftError::AuthInvalid));
        crate::set_gate(false);
    }

    #[tokio::test]
    async fn list_forms_rate_limit_surfaces_remaining() {
        let _g = crate::gate_test_lock().await;
        crate::set_gate(true);
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("GET", "/forms")
            .with_status(429)
            .with_header("x-ratelimit-remaining", "0")
            .with_body("{}")
            .create_async()
            .await;
        let c = DeftClient::new(sample_auth(&server.url())).unwrap();
        match c.list_forms().await.unwrap_err() {
            DeftError::RateLimited { remaining } => assert_eq!(remaining, Some(0)),
            other => panic!("expected RateLimited, got {other:?}"),
        }
        crate::set_gate(false);
    }
}
