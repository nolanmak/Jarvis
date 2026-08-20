//! Cerebras reasoner adapter (#655/#663) — last-resort tier, TEXT-ONLY.
//!
//! Plan A (riding the codex CLI via a custom `model_provider`) died on
//! 2026-08-19 during live verification: codex ≥0.148 removed
//! `wire_api = "chat"` (openai/codex discussion #7782) and Cerebras exposes
//! no Responses API (`POST /v1/responses` → 404). So this is the documented
//! plan B: a thin OpenAI-compatible `chat/completions` client.
//!
//! Why this does NOT violate the no-dual-paths constraint: the eligibility
//! policy (#658) restricts Cerebras to [`CapabilityClass::TextOnly`] presets
//! — pure prompt→text transforms with no tools, no MCP, no hooks. For that
//! class a chat completion IS the whole harness contract, so the adapter is
//! a complete implementation of the same `Reasoner` trait, not a degraded
//! parallel pipeline. Anything tool-shaped never routes here.
//!
//! Auth: `CEREBRAS_API_KEY` JIT-loaded from the keyring per call (#128
//! posture — the key is never put in a spawned env or the SAFELIST; there is
//! no subprocess at all). NB the legacy Node daemon shares this account, so
//! rate buckets are shared too (#668 recommends a dedicated key).

use async_trait::async_trait;
use tracing::warn;

use crate::providers::{model_for, tier_of, ProviderKind};
use crate::reasoner::{reasoner_timeout, Reasoner, ReasonerError, ReasonerOpts};

/// Base URL override — tests point this at a local stub server; production
/// default is the public API.
pub fn cerebras_base_url() -> String {
    std::env::var("AUGMENTAGENT_CEREBRAS_BASE_URL")
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "https://api.cerebras.ai/v1".into())
}

pub struct CerebrasHttpReasoner {
    client: reqwest::Client,
    /// Test override; production resolves [`cerebras_base_url`] per call.
    base: Option<String>,
}

impl Default for CerebrasHttpReasoner {
    fn default() -> Self {
        Self {
            client: reqwest::Client::new(),
            base: None,
        }
    }
}

impl CerebrasHttpReasoner {
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    fn with_base(base: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            base: Some(base),
        }
    }

    async fn call_once(&self, opts: &ReasonerOpts, user_message: &str) -> anyhow::Result<String> {
        let model = model_for(ProviderKind::Cerebras, tier_of(opts));
        let Some(key) = crate::secret_loader::load_provider_key("CEREBRAS_API_KEY") else {
            return Err(anyhow::Error::new(ReasonerError::Local {
                message: "cerebras: CEREBRAS_API_KEY not in keyring or env".into(),
            }));
        };
        let effective_system = if opts.system_prompt.trim().is_empty() {
            "You are a concise assistant."
        } else {
            &opts.system_prompt
        };
        // Text-only wire: `IMAGE:` markers degrade to an honest note (see
        // crate::images).
        let user_message = &crate::images::strip_markers_with_note(user_message);
        let body = serde_json::json!({
            "model": model,
            "messages": [
                {"role": "system", "content": effective_system},
                {"role": "user", "content": user_message},
            ],
            "stream": false,
        });

        let base = self.base.clone().unwrap_or_else(cerebras_base_url);
        let url = format!("{base}/chat/completions");
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&key)
            .json(&body)
            .timeout(reasoner_timeout())
            .send()
            .await
            .map_err(|e| {
                // reqwest timeout / connect errors are provider-side.
                if e.is_timeout() {
                    anyhow::Error::new(ReasonerError::Timeout {
                        provider: "cerebras".into(),
                        secs: reasoner_timeout().as_secs(),
                    })
                } else {
                    anyhow::Error::new(ReasonerError::Unavailable {
                        provider: "cerebras".into(),
                        message: format!("request failed: {e}"),
                    })
                }
            })?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            let detail: String = text.chars().take(300).collect();
            warn!("cerebras HTTP {status}: {detail}");
            // 429 = rate bucket, 402 = credits exhausted — both are spend
            // walls the latch should back off from identically.
            if status.as_u16() == 429 || status.as_u16() == 402 {
                return Err(anyhow::Error::new(ReasonerError::RateLimited {
                    provider: "cerebras".into(),
                    message: format!("HTTP {status}: {detail}"),
                    reset_at: None,
                }));
            }
            if status.as_u16() == 401 || status.as_u16() == 403 {
                return Err(anyhow::Error::new(ReasonerError::Local {
                    message: format!("cerebras auth rejected (HTTP {status}) — check the key"),
                }));
            }
            return Err(anyhow::Error::new(ReasonerError::Unavailable {
                provider: "cerebras".into(),
                message: format!("HTTP {status}: {detail}"),
            }));
        }

        let parsed: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
            anyhow::Error::new(ReasonerError::Unavailable {
                provider: "cerebras".into(),
                message: format!("unparseable completion body: {e}"),
            })
        })?;
        let content = parsed
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if content.is_empty() {
            return Err(anyhow::Error::new(ReasonerError::Unavailable {
                provider: "cerebras".into(),
                message: "completion had no message content".into(),
            }));
        }
        Ok(content)
    }
}

#[async_trait]
impl Reasoner for CerebrasHttpReasoner {
    async fn call(&self, opts: &ReasonerOpts, user_message: &str) -> anyhow::Result<String> {
        self.call_once(opts, user_message).await
    }
    // call_transcript default (→ call) is correct: a chat completion has
    // exactly one final message, so LastBlock == AllBlocks here.
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Minimal one-shot HTTP server: accept one connection, read the
    /// request, respond with `status` + `body`, exit. Enough to exercise the
    /// adapter without an http-server dev-dependency.
    async fn one_shot_server(status: &'static str, body: &'static str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 65536];
            // Read until the end of headers + body enough for the test.
            let _ = sock.read(&mut buf).await;
            let resp = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.shutdown().await;
        });
        format!("http://{addr}/v1")
    }

    fn opts() -> ReasonerOpts {
        ReasonerOpts {
            system_prompt: "You parse loops.".into(),
            model: Some("claude-haiku-4-5-20251001".into()),
            allowed_tools: vec![],
            add_dirs: vec![],
            permission_mode: "default".into(),
            cwd: None,
            env: vec![],
            settings_json: None,
            restrict_env: false,
            audit_logger: None,
            audit_notifier: None,
            session_id: None,
        }
    }

    // NB: the API key resolves from this box's keyring (real key) or the
    // CEREBRAS_API_KEY env on keyring-less machines; the stub server ignores
    // auth either way, and nothing below prints or asserts the key value.

    #[tokio::test]
    async fn parses_chat_completion_content() {
        std::env::set_var("CEREBRAS_API_KEY", "test-key");
        let base = one_shot_server(
            "200 OK",
            r#"{"choices":[{"message":{"role":"assistant","content":"{\"interval_secs\":300}"}}]}"#,
        )
        .await;
        let r = CerebrasHttpReasoner::with_base(base);
        let got = r.call(&opts(), "5m do the digest").await.unwrap();
        assert_eq!(got, "{\"interval_secs\":300}");
    }

    #[tokio::test]
    async fn http_429_maps_to_rate_limited_and_402_too() {
        std::env::set_var("CEREBRAS_API_KEY", "test-key");
        for (status, body) in [
            ("429 Too Many Requests", r#"{"message":"tokens per minute exceeded"}"#),
            ("402 Payment Required", r#"{"message":"insufficient credits"}"#),
        ] {
            let base = one_shot_server(status, body).await;
            let r = CerebrasHttpReasoner::with_base(base);
            let err = r.call(&opts(), "hi").await.unwrap_err();
            assert!(
                matches!(
                    ReasonerError::find_in(&err),
                    Some(ReasonerError::RateLimited { .. })
                ),
                "{status} must map to RateLimited, got: {err:#}"
            );
        }
    }

    #[tokio::test]
    async fn server_error_maps_to_unavailable() {
        std::env::set_var("CEREBRAS_API_KEY", "test-key");
        let base = one_shot_server("503 Service Unavailable", r#"{"message":"overloaded"}"#).await;
        let r = CerebrasHttpReasoner::with_base(base);
        let err = r.call(&opts(), "hi").await.unwrap_err();
        assert!(matches!(
            ReasonerError::find_in(&err),
            Some(ReasonerError::Unavailable { .. })
        ));
    }
}
