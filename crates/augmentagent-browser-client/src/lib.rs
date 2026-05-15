//! Rust client for the AugmentAgent Python browser sidecar.
//!
//! Connects to the sidecar's Unix socket
//! (`${XDG_RUNTIME_DIR}/augmentagent/browser.sock`), frames requests as
//! NDJSON, and returns typed responses. Multiple concurrent in-flight
//! requests on a single connection are supported via a background reader
//! task that demultiplexes responses by `request_id` to per-call oneshot
//! channels.
//!
//! See [issue #75](https://github.com/nolanmak/AugmentAgent/issues/75) §6
//! for the wire protocol.
//!
//! # Example
//!
//! ```no_run
//! use augmentagent_browser_client::{BrowserClient, default_socket_path};
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let client = BrowserClient::connect(default_socket_path()).await?;
//! client.ping().await?;
//! client.navigate("https://twitter.com/home").await?;
//! let _png = client.screenshot("/tmp/twitter.png").await?;
//! # Ok(())
//! # }
//! ```

pub mod cdp;
pub mod cookies;
pub mod session;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{oneshot, Mutex};
use tokio::task::JoinHandle;
use uuid::Uuid;

/// Default socket path resolution. Honors `AUGMENTAGENT_BROWSER_SOCK`,
/// then `${XDG_RUNTIME_DIR}/augmentagent/browser.sock`, finally
/// `/run/user/<uid>/augmentagent/browser.sock`.
pub fn default_socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("AUGMENTAGENT_BROWSER_SOCK") {
        return PathBuf::from(p);
    }
    let runtime = std::env::var("XDG_RUNTIME_DIR")
        .unwrap_or_else(|_| format!("/run/user/{}", unsafe { libc_getuid() }));
    PathBuf::from(runtime).join("augmentagent").join("browser.sock")
}

#[allow(non_snake_case)]
unsafe fn libc_getuid() -> u32 {
    // Avoid pulling in libc just for getuid(); fall back to env or 1000.
    std::env::var("UID")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1000)
}

/// Errors returned by [`BrowserClient`]. The `Sidecar` variant carries the
/// typed `kind` string from the sidecar's error envelope, so callers can
/// branch on `AuthRequired` / `CaptchaDetected` / `ChromiumDisconnected` /
/// etc. without parsing the message.
#[derive(Debug, Error)]
pub enum BrowserError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json decode: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("sidecar error [{kind}]: {message}")]
    Sidecar {
        kind: String,
        message: String,
        page_url: Option<String>,
        screenshot_b64: Option<String>,
    },
    #[error("sidecar disconnected")]
    Disconnected,
    #[error("base64 decode: {0}")]
    Base64(#[from] base64::DecodeError),
}

impl BrowserError {
    /// Convenience: did the sidecar tell us we need a human to log in?
    pub fn is_auth_required(&self) -> bool {
        matches!(self, BrowserError::Sidecar { kind, .. } if kind == "AuthRequired")
    }
    /// Convenience: did the sidecar see a captcha?
    pub fn is_captcha(&self) -> bool {
        matches!(self, BrowserError::Sidecar { kind, .. } if kind == "CaptchaDetected")
    }
    /// Convenience: did Chromium drop the CDP connection?
    pub fn is_chromium_disconnected(&self) -> bool {
        matches!(self, BrowserError::Sidecar { kind, .. } if kind == "ChromiumDisconnected")
    }
}

/// Connection-pooled browser client. Cheap to clone — the underlying
/// connection state is shared via `Arc`.
#[derive(Clone)]
pub struct BrowserClient {
    inner: Arc<Inner>,
}

struct Inner {
    writer: Mutex<tokio::net::unix::OwnedWriteHalf>,
    pending: Arc<DashMap<String, oneshot::Sender<Response>>>,
    _reader_task: JoinHandle<()>,
}

#[derive(Serialize)]
struct Request<'a> {
    request_id: String,
    op: &'a str,
    params: serde_json::Value,
    timeout_ms: u64,
}

#[derive(Debug, Deserialize)]
struct Response {
    request_id: String,
    ok: bool,
    #[serde(default)]
    result: serde_json::Value,
    #[serde(default)]
    error: Option<ErrorEnvelope>,
    #[serde(default)]
    #[allow(dead_code)]
    elapsed_ms: u64,
}

#[derive(Debug, Deserialize)]
struct ErrorEnvelope {
    kind: String,
    message: String,
    #[serde(default)]
    page_url: Option<String>,
    #[serde(default)]
    screenshot_b64: Option<String>,
}

impl BrowserClient {
    /// Connect to the sidecar Unix socket. Spawns a background reader task
    /// that demultiplexes responses by `request_id`.
    pub async fn connect<P: AsRef<Path>>(sock: P) -> Result<Self, BrowserError> {
        let stream = UnixStream::connect(sock).await?;
        let (read_half, write_half) = stream.into_split();
        let reader = BufReader::new(read_half);
        let pending: Arc<DashMap<String, oneshot::Sender<Response>>> = Arc::new(DashMap::new());

        let pending_for_task = Arc::clone(&pending);
        let reader_task = tokio::spawn(async move {
            let mut reader = reader;
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!("browser sidecar read error: {e}");
                        break;
                    }
                }
                match serde_json::from_str::<Response>(line.trim_end()) {
                    Ok(resp) => {
                        if let Some((_, tx)) = pending_for_task.remove(&resp.request_id) {
                            let _ = tx.send(resp);
                        } else {
                            tracing::warn!(
                                request_id = %resp.request_id,
                                "browser sidecar response with no pending request"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!("browser sidecar bad frame: {e} :: {line:?}");
                    }
                }
            }
            // On disconnect, drop all pending senders so callers see Disconnected.
            pending_for_task.clear();
        });

        Ok(Self {
            inner: Arc::new(Inner {
                writer: Mutex::new(write_half),
                pending,
                _reader_task: reader_task,
            }),
        })
    }

    async fn call(
        &self,
        op: &str,
        params: serde_json::Value,
        timeout_ms: u64,
    ) -> Result<serde_json::Value, BrowserError> {
        let request_id = Uuid::new_v4().to_string();
        let req = Request {
            request_id: request_id.clone(),
            op,
            params,
            timeout_ms,
        };
        let mut buf = serde_json::to_vec(&req)?;
        buf.push(b'\n');

        let (tx, rx) = oneshot::channel();
        self.inner.pending.insert(request_id.clone(), tx);

        // Hold the writer lock only for the write itself; concurrent calls
        // may interleave responses (handled by request_id demux).
        {
            let mut w = self.inner.writer.lock().await;
            if let Err(e) = w.write_all(&buf).await {
                self.inner.pending.remove(&request_id);
                return Err(e.into());
            }
        }

        let resp = rx.await.map_err(|_| BrowserError::Disconnected)?;
        if resp.ok {
            Ok(resp.result)
        } else {
            let env = resp.error.unwrap_or(ErrorEnvelope {
                kind: "Internal".into(),
                message: "missing error envelope".into(),
                page_url: None,
                screenshot_b64: None,
            });
            Err(BrowserError::Sidecar {
                kind: env.kind,
                message: env.message,
                page_url: env.page_url,
                screenshot_b64: env.screenshot_b64,
            })
        }
    }

    /// Liveness probe. Round-trips through the asyncio loop but does NOT
    /// touch Chromium — won't trigger lazy attach.
    pub async fn ping(&self) -> Result<(), BrowserError> {
        // ping is one of the few ops that doesn't need the browser; use a
        // short timeout. The sidecar still goes through `_ensure_browser`
        // for ping (it's cheap once attached, and a useful liveness check),
        // but we keep the timeout generous enough to cover the first attach.
        self.call("ping", serde_json::json!({}), 5_000).await?;
        Ok(())
    }

    /// Navigate to `url`. Waits until DOMContentLoaded by default.
    pub async fn navigate(&self, url: &str) -> Result<(), BrowserError> {
        self.call(
            "navigate",
            serde_json::json!({ "url": url, "wait_until": "domcontentloaded" }),
            30_000,
        )
        .await?;
        Ok(())
    }

    /// Click a CSS selector.
    pub async fn click(&self, selector: &str) -> Result<(), BrowserError> {
        self.call(
            "click",
            serde_json::json!({ "selector": selector }),
            10_000,
        )
        .await?;
        Ok(())
    }

    /// Fill a form field. `submit=true` presses Enter after typing.
    pub async fn type_text(
        &self,
        selector: &str,
        text: &str,
    ) -> Result<(), BrowserError> {
        self.call(
            "type",
            serde_json::json!({ "selector": selector, "text": text, "submit": false }),
            10_000,
        )
        .await?;
        Ok(())
    }

    /// Variant that submits (presses Enter) after typing.
    pub async fn type_and_submit(
        &self,
        selector: &str,
        text: &str,
    ) -> Result<(), BrowserError> {
        self.call(
            "type",
            serde_json::json!({ "selector": selector, "text": text, "submit": true }),
            10_000,
        )
        .await?;
        Ok(())
    }

    /// Take a screenshot and write it to `path`. Returns the PNG bytes too.
    pub async fn screenshot<P: AsRef<Path>>(
        &self,
        path: P,
    ) -> Result<Vec<u8>, BrowserError> {
        let path_ref = path.as_ref();
        let v = self
            .call(
                "screenshot",
                serde_json::json!({
                    "full_page": false,
                    "path": path_ref.to_string_lossy(),
                }),
                15_000,
            )
            .await?;
        let b64 = v["b64"].as_str().ok_or_else(|| BrowserError::Sidecar {
            kind: "Internal".into(),
            message: "screenshot response missing b64".into(),
            page_url: None,
            screenshot_b64: None,
        })?;
        let bytes = base64::engine::general_purpose::STANDARD.decode(b64)?;
        Ok(bytes)
    }

    /// Variant that returns bytes only (no server-side save).
    pub async fn screenshot_bytes(&self, full_page: bool) -> Result<Vec<u8>, BrowserError> {
        let v = self
            .call(
                "screenshot",
                serde_json::json!({ "full_page": full_page }),
                15_000,
            )
            .await?;
        let b64 = v["b64"].as_str().ok_or_else(|| BrowserError::Sidecar {
            kind: "Internal".into(),
            message: "screenshot response missing b64".into(),
            page_url: None,
            screenshot_b64: None,
        })?;
        Ok(base64::engine::general_purpose::STANDARD.decode(b64)?)
    }

    /// Read `innerText` of the first element matching `selector`.
    pub async fn get_text(&self, selector: &str) -> Result<String, BrowserError> {
        let v = self
            .call(
                "get_text",
                serde_json::json!({ "selector": selector, "limit": 16_384 }),
                10_000,
            )
            .await?;
        Ok(v["text"].as_str().unwrap_or_default().to_string())
    }

    /// Set files on a `<input type=file>` element.
    pub async fn set_input_files<P: AsRef<Path>>(
        &self,
        selector: &str,
        paths: &[P],
    ) -> Result<(), BrowserError> {
        let paths_json: Vec<String> = paths
            .iter()
            .map(|p| p.as_ref().to_string_lossy().into_owned())
            .collect();
        self.call(
            "set_input_files",
            serde_json::json!({ "selector": selector, "paths": paths_json }),
            15_000,
        )
        .await?;
        Ok(())
    }

    /// Wait for a selector to reach `state` (default `visible`).
    pub async fn wait_for(
        &self,
        selector: &str,
        timeout_ms: u64,
    ) -> Result<(), BrowserError> {
        self.call(
            "wait_for",
            serde_json::json!({
                "selector": selector,
                "state": "visible",
                "timeout_ms": timeout_ms,
            }),
            // Outer timeout = inner + grace. Sidecar enforces inner.
            timeout_ms + 2_000,
        )
        .await?;
        Ok(())
    }

    /// Run `js` in the page and return its (JSON-able) value.
    pub async fn evaluate(&self, js: &str) -> Result<serde_json::Value, BrowserError> {
        let v = self
            .call("evaluate", serde_json::json!({ "js": js }), 10_000)
            .await?;
        Ok(v["value"].clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Smoke test that the request envelope serializes the way the sidecar
    // expects — guards against accidental field renames.
    #[test]
    fn request_envelope_shape() {
        let req = Request {
            request_id: "abc".into(),
            op: "ping",
            params: serde_json::json!({}),
            timeout_ms: 1000,
        };
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("\"request_id\":\"abc\""));
        assert!(s.contains("\"op\":\"ping\""));
        assert!(s.contains("\"timeout_ms\":1000"));
    }

    #[test]
    fn error_envelope_kinds_round_trip() {
        let raw = r#"{"request_id":"x","ok":false,"error":{"kind":"AuthRequired","message":"login"},"elapsed_ms":12}"#;
        let resp: Response = serde_json::from_str(raw).unwrap();
        assert!(!resp.ok);
        let env = resp.error.unwrap();
        assert_eq!(env.kind, "AuthRequired");
        assert_eq!(env.message, "login");
    }

    #[test]
    fn default_socket_path_uses_runtime() {
        std::env::set_var("XDG_RUNTIME_DIR", "/tmp/xdgtest");
        std::env::remove_var("AUGMENTAGENT_BROWSER_SOCK");
        let p = default_socket_path();
        assert_eq!(p, std::path::PathBuf::from("/tmp/xdgtest/augmentagent/browser.sock"));
    }
}
