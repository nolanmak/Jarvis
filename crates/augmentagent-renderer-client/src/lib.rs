//! Rust client for the AugmentAgent Node renderer sidecar.
//!
//! Connects to the sidecar's Unix socket
//! (`${XDG_RUNTIME_DIR}/augmentagent/renderer.sock`), frames requests as
//! NDJSON, and returns typed responses. Multiple concurrent in-flight
//! requests on a single connection are supported via a background reader
//! task that demultiplexes responses by `request_id` to per-call oneshot
//! channels.
//!
//! The wire envelope is identical to the browser sidecar
//! (`crates/augmentagent-browser-client`) — same `request_id` / `op` /
//! `params` / `timeout_ms` request frame and `ok` / `result` / `error` /
//! `elapsed_ms` response frame. See `sidecars/renderer/server.mjs` and
//! `docs/REMOTION.md` for the protocol.
//!
//! # Example
//!
//! ```no_run
//! use augmentagent_renderer_client::{RendererClient, default_socket_path};
//! # async fn run() -> Result<(), Box<dyn std::error::Error>> {
//! let client = RendererClient::connect(default_socket_path()).await?;
//! client.ping().await?;
//! let out = client
//!     .render(
//!         serde_json::json!({ "title": "Hi", "body": "Body", "durationSec": 6 }),
//!         "/tmp/out.mp4",
//!     )
//!     .await?;
//! println!("{} bytes -> {}", out.bytes, out.path);
//! # Ok(())
//! # }
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{oneshot, Mutex};
use tokio::task::JoinHandle;
use uuid::Uuid;

/// Default socket path resolution. Honors `AUGMENTAGENT_RENDERER_SOCK`,
/// then `${XDG_RUNTIME_DIR}/augmentagent/renderer.sock`, finally
/// `/run/user/<uid>/augmentagent/renderer.sock`.
pub fn default_socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("AUGMENTAGENT_RENDERER_SOCK") {
        return PathBuf::from(p);
    }
    let runtime = std::env::var("XDG_RUNTIME_DIR")
        .unwrap_or_else(|_| format!("/run/user/{}", fallback_uid()));
    PathBuf::from(runtime)
        .join("augmentagent")
        .join("renderer.sock")
}

fn fallback_uid() -> u32 {
    // Avoid pulling in libc just for getuid(); fall back to env or 1000.
    std::env::var("UID")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1000)
}

/// Errors returned by [`RendererClient`]. The `Sidecar` variant carries the
/// typed `kind` string from the sidecar's error envelope, so callers can
/// branch on `BadProps` / `RenderFailed` / `BundleFailed` / `Timeout`
/// without parsing the message.
#[derive(Debug, Error)]
pub enum RendererError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json decode: {0}")]
    Decode(#[from] serde_json::Error),
    #[error("sidecar error [{kind}]: {message}")]
    Sidecar { kind: String, message: String },
    #[error("sidecar disconnected")]
    Disconnected,
}

impl RendererError {
    /// Convenience: did the sidecar reject the input props?
    pub fn is_bad_props(&self) -> bool {
        matches!(self, RendererError::Sidecar { kind, .. } if kind == "BadProps")
    }
    /// Convenience: did the Remotion render itself fail?
    pub fn is_render_failed(&self) -> bool {
        matches!(self, RendererError::Sidecar { kind, .. } if kind == "RenderFailed")
    }
    /// Convenience: did the Remotion bundle fail to build?
    pub fn is_bundle_failed(&self) -> bool {
        matches!(self, RendererError::Sidecar { kind, .. } if kind == "BundleFailed")
    }
    /// Convenience: did the op time out?
    pub fn is_timeout(&self) -> bool {
        matches!(self, RendererError::Sidecar { kind, .. } if kind == "Timeout")
    }
}

/// Result of a successful [`RendererClient::render`].
#[derive(Debug, Clone, Deserialize)]
pub struct RenderOutput {
    /// Absolute (or as-passed) path the mp4 was written to.
    pub path: String,
    /// Size of the rendered file in bytes.
    pub bytes: u64,
    /// Server-side wall time for the render itself (ms).
    pub duration_ms: u64,
}

/// Connection-pooled renderer client. Cheap to clone — the underlying
/// connection state is shared via `Arc`.
#[derive(Clone)]
pub struct RendererClient {
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
}

/// Default render timeout. A 1080×1920 clip of a few hundred frames takes
/// well under this on this box; the first render also pays the bundle cost.
pub const DEFAULT_RENDER_TIMEOUT_MS: u64 = 300_000;

impl RendererClient {
    /// Connect to the sidecar Unix socket. Spawns a background reader task
    /// that demultiplexes responses by `request_id`.
    pub async fn connect<P: AsRef<Path>>(sock: P) -> Result<Self, RendererError> {
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
                        tracing::warn!("renderer sidecar read error: {e}");
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
                                "renderer sidecar response with no pending request"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!("renderer sidecar bad frame: {e} :: {line:?}");
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
    ) -> Result<serde_json::Value, RendererError> {
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

        let resp = rx.await.map_err(|_| RendererError::Disconnected)?;
        if resp.ok {
            Ok(resp.result)
        } else {
            let env = resp.error.unwrap_or(ErrorEnvelope {
                kind: "Internal".into(),
                message: "missing error envelope".into(),
            });
            Err(RendererError::Sidecar {
                kind: env.kind,
                message: env.message,
            })
        }
    }

    /// Liveness probe. Round-trips through the Node event loop but does NOT
    /// touch Remotion — won't trigger a bundle/render.
    pub async fn ping(&self) -> Result<(), RendererError> {
        self.call("ping", serde_json::json!({}), 5_000).await?;
        Ok(())
    }

    /// Render the `ShortCard` composition with `props`, writing an mp4 to
    /// `out_path`. h264 codec, default timeout. Returns the output metadata.
    pub async fn render<P: AsRef<Path>>(
        &self,
        props: serde_json::Value,
        out_path: P,
    ) -> Result<RenderOutput, RendererError> {
        self.render_with(props, out_path, "h264", DEFAULT_RENDER_TIMEOUT_MS)
            .await
    }

    /// Render with an explicit codec and timeout.
    pub async fn render_with<P: AsRef<Path>>(
        &self,
        props: serde_json::Value,
        out_path: P,
        codec: &str,
        timeout_ms: u64,
    ) -> Result<RenderOutput, RendererError> {
        let out = out_path.as_ref().to_string_lossy().into_owned();
        let v = self
            .call(
                "render",
                serde_json::json!({
                    "props": props,
                    "out_path": out,
                    "codec": codec,
                }),
                timeout_ms,
            )
            .await?;
        serde_json::from_value(v).map_err(RendererError::Decode)
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
            op: "render",
            params: serde_json::json!({ "out_path": "/tmp/x.mp4" }),
            timeout_ms: 120_000,
        };
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("\"request_id\":\"abc\""));
        assert!(s.contains("\"op\":\"render\""));
        assert!(s.contains("\"timeout_ms\":120000"));
    }

    #[test]
    fn error_envelope_kinds_round_trip() {
        let raw = r#"{"request_id":"x","ok":false,"error":{"kind":"RenderFailed","message":"boom"},"elapsed_ms":12}"#;
        let resp: Response = serde_json::from_str(raw).unwrap();
        assert!(!resp.ok);
        let env = resp.error.unwrap();
        assert_eq!(env.kind, "RenderFailed");
        assert_eq!(env.message, "boom");
    }

    #[test]
    fn success_envelope_decodes_render_output() {
        let raw = r#"{"request_id":"x","ok":true,"result":{"path":"/tmp/a.mp4","bytes":4096,"duration_ms":8123},"elapsed_ms":8200}"#;
        let resp: Response = serde_json::from_str(raw).unwrap();
        assert!(resp.ok);
        let out: RenderOutput = serde_json::from_value(resp.result).unwrap();
        assert_eq!(out.path, "/tmp/a.mp4");
        assert_eq!(out.bytes, 4096);
        assert_eq!(out.duration_ms, 8123);
    }

    #[test]
    fn error_helpers_classify_kind() {
        let e = RendererError::Sidecar {
            kind: "BadProps".into(),
            message: "no out_path".into(),
        };
        assert!(e.is_bad_props());
        assert!(!e.is_render_failed());
        assert!(!e.is_timeout());

        let e2 = RendererError::Sidecar {
            kind: "Timeout".into(),
            message: "slow".into(),
        };
        assert!(e2.is_timeout());
        assert!(!e2.is_bundle_failed());
    }

    #[test]
    fn default_socket_path_uses_runtime() {
        std::env::set_var("XDG_RUNTIME_DIR", "/tmp/xdgtest-renderer");
        std::env::remove_var("AUGMENTAGENT_RENDERER_SOCK");
        let p = default_socket_path();
        assert_eq!(
            p,
            std::path::PathBuf::from("/tmp/xdgtest-renderer/augmentagent/renderer.sock")
        );
    }

    #[test]
    fn explicit_socket_env_overrides() {
        std::env::set_var("AUGMENTAGENT_RENDERER_SOCK", "/tmp/custom.sock");
        let p = default_socket_path();
        assert_eq!(p, std::path::PathBuf::from("/tmp/custom.sock"));
        std::env::remove_var("AUGMENTAGENT_RENDERER_SOCK");
    }
}
