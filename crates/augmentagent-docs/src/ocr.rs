//! Thin Mistral OCR client (#939): one `POST /v1/ocr` with the document inline
//! as a base64 data URI. Accepts PDFs (and images) directly — no rasterization
//! step — and returns per-page markdown, which we join in page order.
//!
//! Auth: `MISTRAL_API_KEY`, keyring first (service slot `api-key`, the same
//! slot `augmentagent migrate-secrets-to-keyring` fills for the other provider
//! keys), then the process env (`.env` via dotenvy in `main`). Model and base
//! URL are env-overridable so tests can point at a mock and the owner can pin
//! a newer OCR model without a rebuild.

use std::time::Duration;

use base64::Engine;
use serde::Deserialize;
use tracing::{debug, warn};

use crate::{resolve_api_key, MISTRAL_API_KEY_ENV, OCR_BASE_URL_ENV, OCR_MODEL_ENV};

/// Mistral's alias for the current OCR model.
pub const DEFAULT_OCR_MODEL: &str = "mistral-ocr-latest";
/// Production API root.
pub const DEFAULT_OCR_BASE_URL: &str = "https://api.mistral.ai";
/// Mistral's documented per-document limit (50 MB). Anything larger is
/// refused locally before we base64 it into memory.
pub const MAX_OCR_BYTES: usize = 50 * 1024 * 1024;

/// Keyring slot shared with the other provider keys (see
/// `augmentagent_channel_core::secret_loader::SECRET_SERVICE_API_KEY`; the
/// literal is mirrored here because this crate must stay below core).
const SECRET_SERVICE_API_KEY: &str = "api-key";

/// A scanned multi-page report takes single-digit seconds; leave headroom for
/// a slow upstream without hanging the Discord handler forever.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(180);

/// One OCR run: page markdown joined in index order, plus the page count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OcrResult {
    pub markdown: String,
    pub pages: u32,
}

#[derive(Debug, Clone)]
pub struct OcrClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    max_bytes: usize,
}

impl OcrClient {
    /// Client with the default base URL and model (each env-overridable via
    /// [`OCR_BASE_URL_ENV`] / [`OCR_MODEL_ENV`]).
    pub fn new(api_key: String) -> Self {
        let env_or = |key: &str, default: &str| {
            std::env::var(key)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| default.to_string())
        };
        Self {
            http: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .unwrap_or_default(),
            base_url: env_or(OCR_BASE_URL_ENV, DEFAULT_OCR_BASE_URL),
            api_key,
            model: env_or(OCR_MODEL_ENV, DEFAULT_OCR_MODEL),
            max_bytes: MAX_OCR_BYTES,
        }
    }

    /// Build from the live key sources. `None` (and a one-line debug log)
    /// when no key is configured — the pipeline then skips OCR entirely.
    pub fn from_env() -> Option<Self> {
        let keyring =
            match augmentagent_auth::Auth::get(SECRET_SERVICE_API_KEY, MISTRAL_API_KEY_ENV) {
                Ok(bytes) => match String::from_utf8(bytes) {
                    Ok(s) => Some(s),
                    Err(_) => {
                        warn!(
                            key = MISTRAL_API_KEY_ENV,
                            "keyring entry is not valid UTF-8; ignoring"
                        );
                        None
                    }
                },
                Err(augmentagent_auth::AuthError::NotFound { .. }) => None,
                Err(e) => {
                    // A broken keyring backend must not disable OCR on a box that
                    // has the key in .env; fall through to env.
                    debug!(
                        key = MISTRAL_API_KEY_ENV,
                        "keyring lookup failed ({e}); trying env"
                    );
                    None
                }
            };
        let env = std::env::var(MISTRAL_API_KEY_ENV).ok();
        match resolve_api_key(keyring, env) {
            Some(key) => Some(Self::new(key)),
            None => {
                debug!(key = MISTRAL_API_KEY_ENV, "not set; OCR fallback disabled");
                None
            }
        }
    }

    pub fn with_base_url(mut self, base_url: String) -> Self {
        self.base_url = base_url.trim_end_matches('/').to_string();
        self
    }

    pub fn with_model(mut self, model: String) -> Self {
        self.model = model;
        self
    }

    /// Lower the size cap (tests).
    pub fn with_max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// OCR a PDF given its raw bytes.
    pub async fn ocr_pdf(&self, bytes: &[u8]) -> anyhow::Result<OcrResult> {
        self.run("document_url", "application/pdf", bytes).await
    }

    /// OCR a single image (`image/png`, `image/jpeg`, …).
    pub async fn ocr_image(&self, bytes: &[u8], mime: &str) -> anyhow::Result<OcrResult> {
        self.run("image_url", mime, bytes).await
    }

    async fn run(&self, doc_type: &str, mime: &str, bytes: &[u8]) -> anyhow::Result<OcrResult> {
        if bytes.len() > self.max_bytes {
            anyhow::bail!(
                "document too large for OCR: {} bytes (cap {})",
                bytes.len(),
                self.max_bytes
            );
        }
        let data_uri = format!(
            "data:{mime};base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        );
        let body = build_request(&self.model, doc_type, &data_uri);
        let url = format!("{}/v1/ocr", self.base_url);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.api_key)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("mistral ocr request: {e}"))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| anyhow::anyhow!("mistral ocr read body: {e}"))?;
        if !status.is_success() {
            anyhow::bail!("mistral ocr → {status}: {}", text.trim());
        }
        let v: serde_json::Value =
            serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("mistral ocr decode: {e}"))?;
        parse_response(&v)
    }
}

/// Request body per Mistral's OCR API: `{model, document:{type, <type>: uri}}`.
/// The document key is named after its type (`document_url` / `image_url`).
fn build_request(model: &str, doc_type: &str, data_uri: &str) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "document": {
            "type": doc_type,
            doc_type: data_uri,
        },
        "include_image_base64": false,
    })
}

#[derive(Deserialize)]
struct Page {
    #[serde(default)]
    index: u32,
    #[serde(default)]
    markdown: String,
}

/// Join `pages[].markdown` in `index` order. Rejects a body with no `pages`
/// array so a shape drift surfaces as an error rather than an empty document.
fn parse_response(v: &serde_json::Value) -> anyhow::Result<OcrResult> {
    let pages_v = v
        .get("pages")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("mistral ocr response has no `pages` array"))?;
    let mut pages: Vec<Page> = pages_v
        .iter()
        .map(|p| serde_json::from_value(p.clone()))
        .collect::<Result<_, _>>()
        .map_err(|e| anyhow::anyhow!("mistral ocr page decode: {e}"))?;
    pages.sort_by_key(|p| p.index);
    let markdown = pages
        .iter()
        .map(|p| p.markdown.trim())
        .collect::<Vec<_>>()
        .join("\n\n");
    Ok(OcrResult {
        markdown,
        pages: pages.len() as u32,
    })
}
