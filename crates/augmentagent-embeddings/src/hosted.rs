//! Hosted provider (#1131): an OpenAI-compatible `/embeddings` endpoint
//! behind the same `Embedder` trait. **Opt-in only.** It sends message text
//! to a third party, so it is never selected implicitly, never falls back to
//! the local model under the hosted label, and its key is loaded just-in-time
//! from the keyring rather than the environment safelist.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};

use crate::embedder::{l2_normalize, Embedder, Embedding, ModelId};

pub const ENV_HOSTED_MODEL: &str = "AUGMENTAGENT_EMBEDDINGS_HOSTED_MODEL";
pub const ENV_HOSTED_DIM: &str = "AUGMENTAGENT_EMBEDDINGS_HOSTED_DIM";
pub const ENV_HOSTED_BASE_URL: &str = "AUGMENTAGENT_EMBEDDINGS_HOSTED_URL";
/// Keyring account (service `augmentagent/api-key`) and env fallback name.
pub const KEY_NAME: &str = "OPENAI_API_KEY";
pub const DEFAULT_HOSTED_MODEL: &str = "text-embedding-3-large";
pub const DEFAULT_HOSTED_DIM: usize = 1024;
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

#[derive(Debug, Clone)]
pub struct HostedConfig {
    pub base_url: String,
    pub model: String,
    pub dim: usize,
    /// Inputs per request.
    pub max_batch: usize,
    /// Characters per input; longer inputs are truncated (≈ tokens × 4).
    pub max_input_chars: usize,
    /// Hard cap on HTTP requests for one embedder instance (one run).
    pub max_requests: u64,
    pub max_retries: u32,
    /// USD per million tokens, for the cost estimate.
    pub usd_per_million_tokens: f64,
    /// Estimate and count, but send nothing.
    pub dry_run: bool,
}

impl Default for HostedConfig {
    fn default() -> Self {
        Self {
            base_url: std::env::var(ENV_HOSTED_BASE_URL)
                .unwrap_or_else(|_| DEFAULT_BASE_URL.into()),
            model: std::env::var(ENV_HOSTED_MODEL).unwrap_or_else(|_| DEFAULT_HOSTED_MODEL.into()),
            dim: std::env::var(ENV_HOSTED_DIM)
                .ok()
                .and_then(|d| d.parse().ok())
                .unwrap_or(DEFAULT_HOSTED_DIM),
            max_batch: 64,
            max_input_chars: 8_000,
            max_requests: 20_000,
            max_retries: 5,
            usd_per_million_tokens: 0.13,
            dry_run: false,
        }
    }
}

/// Key from the keyring slot `augmentagent/api-key` / `OPENAI_API_KEY`, else
/// the same-named env var (not on any safelist). Never logged.
pub fn load_key() -> Option<String> {
    match augmentagent_auth::Auth::get("api-key", KEY_NAME) {
        Ok(bytes) => String::from_utf8(bytes)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        Err(_) => std::env::var(KEY_NAME)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
    }
}

#[derive(Debug, Default, Clone, Serialize, PartialEq, Eq)]
pub struct Usage {
    pub requests: u64,
    pub inputs: u64,
    /// Tokens as reported by the API (or estimated in dry-run: chars / 4).
    pub tokens: u64,
}

pub struct HostedEmbedder {
    id: ModelId,
    cfg: HostedConfig,
    key: Option<String>,
    client: reqwest::blocking::Client,
    requests: AtomicU64,
    inputs: AtomicU64,
    tokens: AtomicU64,
}

impl std::fmt::Debug for HostedEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the key.
        f.debug_struct("HostedEmbedder")
            .field("id", &self.id)
            .field("dry_run", &self.cfg.dry_run)
            .finish()
    }
}

#[derive(Serialize)]
struct Req<'a> {
    model: &'a str,
    input: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    dimensions: Option<usize>,
    encoding_format: &'static str,
}

#[derive(Deserialize)]
struct Resp {
    data: Vec<Datum>,
    #[serde(default)]
    usage: Option<RespUsage>,
}
#[derive(Deserialize)]
struct Datum {
    index: usize,
    embedding: Vec<f32>,
}
#[derive(Deserialize)]
struct RespUsage {
    #[serde(default)]
    total_tokens: u64,
}

impl HostedEmbedder {
    /// Construction fails when no key is available (unless `dry_run`): the
    /// hosted provider never silently becomes the local one.
    pub fn new(cfg: HostedConfig, key: Option<String>) -> anyhow::Result<Self> {
        if key.is_none() && !cfg.dry_run {
            bail!(
                "hosted embeddings selected but no key: store it in the keyring as \
                 `augmentagent/api-key` / `{KEY_NAME}` (or set {KEY_NAME}); not falling back to local"
            );
        }
        anyhow::ensure!(cfg.dim > 0, "hosted dim must be positive");
        Ok(Self {
            id: ModelId {
                provider: "hosted".into(),
                model: cfg.model.clone(),
                dim: cfg.dim,
            },
            client: reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(60))
                .build()?,
            cfg,
            key,
            requests: AtomicU64::new(0),
            inputs: AtomicU64::new(0),
            tokens: AtomicU64::new(0),
        })
    }

    pub fn usage(&self) -> Usage {
        Usage {
            requests: self.requests.load(Ordering::Relaxed),
            inputs: self.inputs.load(Ordering::Relaxed),
            tokens: self.tokens.load(Ordering::Relaxed),
        }
    }

    pub fn estimated_cost_usd(&self) -> f64 {
        self.usage().tokens as f64 / 1_000_000.0 * self.cfg.usd_per_million_tokens
    }

    fn truncate(&self, t: &str) -> String {
        if t.len() <= self.cfg.max_input_chars {
            return t.to_string();
        }
        let mut end = self.cfg.max_input_chars;
        while !t.is_char_boundary(end) {
            end -= 1;
        }
        t[..end].to_string()
    }

    fn call(&self, inputs: &[String]) -> anyhow::Result<Vec<Embedding>> {
        if self.cfg.dry_run {
            self.requests.fetch_add(1, Ordering::Relaxed);
            self.inputs
                .fetch_add(inputs.len() as u64, Ordering::Relaxed);
            let est: u64 = inputs.iter().map(|s| (s.len() as u64).div_ceil(4)).sum();
            self.tokens.fetch_add(est, Ordering::Relaxed);
            // Deterministic placeholder so callers can exercise the pipeline.
            return Ok(inputs.iter().map(|_| vec![0.0; self.id.dim]).collect());
        }
        let key = self.key.as_deref().context("hosted embedder has no key")?;
        let url = format!("{}/embeddings", self.cfg.base_url.trim_end_matches('/'));
        let body = Req {
            model: &self.cfg.model,
            input: inputs,
            dimensions: Some(self.cfg.dim),
            encoding_format: "float",
        };
        let mut attempt = 0u32;
        loop {
            if self.requests.load(Ordering::Relaxed) >= self.cfg.max_requests {
                bail!(
                    "hosted embeddings: request cap {} reached for this run",
                    self.cfg.max_requests
                );
            }
            self.requests.fetch_add(1, Ordering::Relaxed);
            let resp = self
                .client
                .post(&url)
                .bearer_auth(key)
                .json(&body)
                .send()
                .with_context(|| format!("POST {url}"))?;
            let status = resp.status();
            if status.as_u16() == 401 || status.as_u16() == 403 {
                bail!("hosted embeddings: authentication failed (HTTP {status}); check the {KEY_NAME} key");
            }
            if status.as_u16() == 429 || status.is_server_error() {
                attempt += 1;
                if attempt > self.cfg.max_retries {
                    bail!("hosted embeddings: HTTP {status} after {attempt} attempts");
                }
                let retry_after = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .map(Duration::from_secs)
                    .unwrap_or_else(|| Duration::from_millis(250 * 2u64.pow(attempt.min(6))));
                std::thread::sleep(retry_after.min(Duration::from_secs(30)));
                continue;
            }
            if !status.is_success() {
                let text = resp.text().unwrap_or_default();
                bail!(
                    "hosted embeddings: HTTP {status}: {}",
                    text.chars().take(300).collect::<String>()
                );
            }
            let parsed: Resp = resp.json().context("parse embeddings response")?;
            anyhow::ensure!(
                parsed.data.len() == inputs.len(),
                "response had {} vectors for {} inputs",
                parsed.data.len(),
                inputs.len()
            );
            self.inputs
                .fetch_add(inputs.len() as u64, Ordering::Relaxed);
            self.tokens.fetch_add(
                parsed.usage.map(|u| u.total_tokens).unwrap_or(0),
                Ordering::Relaxed,
            );
            let mut out: Vec<Option<Embedding>> = vec![None; inputs.len()];
            for d in parsed.data {
                anyhow::ensure!(
                    d.index < inputs.len(),
                    "response index {} out of range",
                    d.index
                );
                anyhow::ensure!(
                    d.embedding.len() == self.id.dim,
                    "response dim {} != configured {}",
                    d.embedding.len(),
                    self.id.dim
                );
                let mut v = d.embedding;
                l2_normalize(&mut v);
                out[d.index] = Some(v);
            }
            return out
                .into_iter()
                .map(|v| v.context("response missing an index"))
                .collect();
        }
    }
}

impl Embedder for HostedEmbedder {
    fn id(&self) -> &ModelId {
        &self.id
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn embed(&self, texts: &[String]) -> anyhow::Result<Vec<Embedding>> {
        let mut out = Vec::with_capacity(texts.len());
        for batch in texts.chunks(self.cfg.max_batch.max(1)) {
            let inputs: Vec<String> = batch.iter().map(|t| self.truncate(t)).collect();
            out.extend(self.call(&inputs)?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg(server: &mockito::ServerGuard) -> HostedConfig {
        HostedConfig {
            base_url: server.url(),
            model: "test-embed".into(),
            dim: 4,
            max_batch: 2,
            max_input_chars: 20,
            max_requests: 10,
            max_retries: 3,
            usd_per_million_tokens: 1.0,
            dry_run: false,
        }
    }

    fn ok_body(n: usize) -> String {
        let data: Vec<_> = (0..n)
            .map(|i| json!({"index": i, "embedding": [1.0, 0.0, 0.0, i as f32]}))
            .collect();
        json!({"data": data, "usage": {"total_tokens": 7 * n}}).to_string()
    }

    #[test]
    fn hosted_without_key_fails_at_construction_and_never_falls_back() {
        let server = mockito::Server::new();
        let err = HostedEmbedder::new(cfg(&server), None)
            .expect_err("no key must fail")
            .to_string();
        assert!(err.contains("not falling back to local"), "{err}");
        assert!(err.contains(KEY_NAME));
        // Dry run needs no key.
        let mut c = cfg(&server);
        c.dry_run = true;
        assert!(HostedEmbedder::new(c, None).is_ok());
    }

    #[test]
    fn batches_respect_max_batch_and_preserve_order() {
        let mut server = mockito::Server::new();
        let m = server
            .mock("POST", "/embeddings")
            .match_header("authorization", "Bearer test-key-value")
            .with_body(ok_body(2))
            .expect(2)
            .create();
        let e = HostedEmbedder::new(cfg(&server), Some("test-key-value".into())).unwrap();
        let out = e
            .embed(&["a".into(), "b".into(), "c".into(), "d".into()])
            .unwrap();
        assert_eq!(out.len(), 4);
        // Response index 1 carries a 1.0 in the last slot → normalized order check.
        assert!(out[1][3] > 0.0 && out[0][3] == 0.0);
        assert!((out[0].iter().map(|x| x * x).sum::<f32>().sqrt() - 1.0).abs() < 1e-5);
        m.assert();
        let u = e.usage();
        assert_eq!((u.requests, u.inputs, u.tokens), (2, 4, 28));
        assert!((e.estimated_cost_usd() - 28.0 / 1_000_000.0).abs() < 1e-12);
    }

    #[test]
    fn retries_on_429_then_succeeds_and_gives_up_after_the_cap() {
        let mut server = mockito::Server::new();
        let _limited = server
            .mock("POST", "/embeddings")
            .with_status(429)
            .with_header("retry-after", "0")
            .expect(1)
            .create();
        let _ok = server
            .mock("POST", "/embeddings")
            .with_body(ok_body(1))
            .expect(1)
            .create();
        let e = HostedEmbedder::new(cfg(&server), Some("k".into())).unwrap();
        assert_eq!(e.embed(&["x".into()]).unwrap().len(), 1);
        assert_eq!(e.usage().requests, 2);

        let mut server = mockito::Server::new();
        let _always = server
            .mock("POST", "/embeddings")
            .with_status(503)
            .expect_at_least(4)
            .create();
        let e = HostedEmbedder::new(cfg(&server), Some("k".into())).unwrap();
        let err = e.embed(&["x".into()]).unwrap_err().to_string();
        assert!(err.contains("after 4 attempts"), "{err}");
    }

    #[test]
    fn auth_failure_is_typed_and_not_retried() {
        let mut server = mockito::Server::new();
        let m = server
            .mock("POST", "/embeddings")
            .with_status(401)
            .expect(1)
            .create();
        let e = HostedEmbedder::new(cfg(&server), Some("bad".into())).unwrap();
        let err = e.embed(&["x".into()]).unwrap_err().to_string();
        assert!(err.contains("authentication failed"), "{err}");
        m.assert();
    }

    #[test]
    fn request_cap_is_hard() {
        let mut server = mockito::Server::new();
        let _ok = server
            .mock("POST", "/embeddings")
            .with_body(ok_body(1))
            .create();
        let mut c = cfg(&server);
        c.max_requests = 2;
        c.max_batch = 1;
        let e = HostedEmbedder::new(c, Some("k".into())).unwrap();
        let err = e
            .embed(&["a".into(), "b".into(), "c".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("request cap 2"), "{err}");
    }

    #[test]
    fn dry_run_sends_nothing_and_estimates_tokens() {
        let mut server = mockito::Server::new();
        let m = server.mock("POST", "/embeddings").expect(0).create();
        let mut c = cfg(&server);
        c.dry_run = true;
        let e = HostedEmbedder::new(c, None).unwrap();
        let out = e.embed(&["twelve chars".into(), "x".into()]).unwrap();
        assert_eq!(out.len(), 2);
        m.assert();
        let u = e.usage();
        assert_eq!(u.inputs, 2);
        assert_eq!(u.tokens, 3 + 1, "chars/4 rounded up");
    }

    #[test]
    fn inputs_are_truncated_and_vectors_record_provider_model_dim() {
        let mut server = mockito::Server::new();
        let m = server
            .mock("POST", "/embeddings")
            .match_body(mockito::Matcher::PartialJson(
                json!({"input": ["01234567890123456789"], "dimensions": 4, "model": "test-embed"}),
            ))
            .with_body(ok_body(1))
            .create();
        let e = HostedEmbedder::new(cfg(&server), Some("k".into())).unwrap();
        e.embed(&["0123456789012345678901234567".into()]).unwrap();
        m.assert();
        assert_eq!(
            e.id(),
            &ModelId {
                provider: "hosted".into(),
                model: "test-embed".into(),
                dim: 4
            }
        );
        assert!(
            !format!("{e:?}").contains('k'),
            "debug output never shows the key"
        );
    }

    #[test]
    fn wrong_dimension_from_the_api_is_an_error() {
        let mut server = mockito::Server::new();
        let _m = server
            .mock("POST", "/embeddings")
            .with_body(json!({"data": [{"index": 0, "embedding": [1.0, 2.0]}]}).to_string())
            .create();
        let e = HostedEmbedder::new(cfg(&server), Some("k".into())).unwrap();
        assert!(e
            .embed(&["x".into()])
            .unwrap_err()
            .to_string()
            .contains("dim 2 != configured 4"));
    }
}
