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

/// The provider's public model catalog (OpenAI-compatible `GET /v1/models`).
/// `doctor --deep` (#658) uses it to flag a pin deprecated out from under us:
/// Cerebras retired five model families in twelve months, and a fallback
/// pinned to a dead id fails every call it serves. The key rides a bearer
/// header and never appears in an error string.
pub async fn list_models(
    client: &reqwest::Client,
    base: &str,
    key: &str,
) -> Result<Vec<String>, String> {
    let url = format!("{}/models", base.trim_end_matches('/'));
    let resp = client
        .get(&url)
        .bearer_auth(key)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err(format!("auth rejected (HTTP {status}) — check the key"));
    }
    if !status.is_success() {
        let detail: String = text.chars().take(300).collect();
        return Err(format!("HTTP {status}: {detail}"));
    }
    let parsed: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("unparseable catalog body: {e}"))?;
    let ids: Vec<String> = parsed
        .get("data")
        .and_then(|d| d.as_array())
        .map(|entries| {
            entries
                .iter()
                .filter_map(|m| m.get("id").and_then(|i| i.as_str()))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if ids.is_empty() {
        // Never real — an unusable answer, not "every pin is gone".
        return Err("catalog listed no models".into());
    }
    Ok(ids)
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
    use crate::providers::ModelTier;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Minimal one-shot HTTP server: accept one connection, read the
    /// request, respond with `status` + `body`, exit. Enough to exercise the
    /// adapter without an http-server dev-dependency. Returns the base URL
    /// plus a handle yielding the raw request the adapter sent.
    async fn one_shot_server(
        status: &'static str,
        body: &'static str,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 65536];
            // Read until the end of headers + body enough for the test.
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let resp = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.shutdown().await;
            String::from_utf8_lossy(&buf[..n]).into_owned()
        });
        (format!("http://{addr}/v1"), seen)
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
        let (base, _seen) = one_shot_server(
            "200 OK",
            r#"{"choices":[{"message":{"role":"assistant","content":"{\"interval_secs\":300}"}}]}"#,
        )
        .await;
        let r = CerebrasHttpReasoner::with_base(base);
        let got = r.call(&opts(), "5m do the digest").await.unwrap();
        assert_eq!(got, "{\"interval_secs\":300}");
    }

    /// #658 — the wire end of the tier map: the posted body carries exactly
    /// the model `model_for` resolved, on BOTH tiers.
    #[tokio::test]
    async fn request_body_pins_the_resolved_model_on_both_tiers() {
        std::env::set_var("CEREBRAS_API_KEY", "test-key");
        for (preset_model, tier) in [
            ("claude-opus-4-8", ModelTier::Quality),
            ("claude-haiku-4-5-20251001", ModelTier::Fast),
        ] {
            let (base, seen) =
                one_shot_server("200 OK", r#"{"choices":[{"message":{"content":"ok"}}]}"#).await;
            let mut o = opts();
            o.model = Some(preset_model.into());
            let r = CerebrasHttpReasoner::with_base(base);
            r.call(&o, "hi").await.unwrap();
            let want = model_for(ProviderKind::Cerebras, tier);
            assert!(
                seen.await.unwrap().contains(&format!(r#""model":"{want}""#)),
                "cerebras must post model {want}"
            );
        }
    }

    #[tokio::test]
    async fn http_429_maps_to_rate_limited_and_402_too() {
        std::env::set_var("CEREBRAS_API_KEY", "test-key");
        for (status, body) in [
            ("429 Too Many Requests", r#"{"message":"tokens per minute exceeded"}"#),
            ("402 Payment Required", r#"{"message":"insufficient credits"}"#),
        ] {
            let (base, _seen) = one_shot_server(status, body).await;
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
    async fn list_models_parses_catalog_ids() {
        let (base, _seen) = one_shot_server(
            "200 OK",
            r#"{"object":"list","data":[{"id":"gpt-oss-120b"},{"id":"gemma-4-31b"}]}"#,
        )
        .await;
        let got = list_models(&reqwest::Client::new(), &base, "test-key")
            .await
            .unwrap();
        assert_eq!(got, vec!["gpt-oss-120b".to_string(), "gemma-4-31b".to_string()]);
    }

    #[tokio::test]
    async fn list_models_reports_auth_failure_without_leaking_the_key() {
        let (base, _seen) =
            one_shot_server("401 Unauthorized", r#"{"message":"wrong api key"}"#).await;
        let err = list_models(&reqwest::Client::new(), &base, "sk-not-a-real-key")
            .await
            .unwrap_err();
        assert!(err.contains("auth"), "401 must read as an auth failure: {err}");
        assert!(
            !err.contains("sk-not-a-real-key"),
            "the key must never reach a doctor finding: {err}"
        );
    }

    #[tokio::test]
    async fn server_error_maps_to_unavailable() {
        std::env::set_var("CEREBRAS_API_KEY", "test-key");
        let (base, _seen) =
            one_shot_server("503 Service Unavailable", r#"{"message":"overloaded"}"#).await;
        let r = CerebrasHttpReasoner::with_base(base);
        let err = r.call(&opts(), "hi").await.unwrap_err();
        assert!(matches!(
            ReasonerError::find_in(&err),
            Some(ReasonerError::Unavailable { .. })
        ));
    }
}
