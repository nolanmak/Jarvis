//! Jarvis Dashboard socket (spec web-dashboard-v2 D2, §5.2): a private Unix
//! socket (file 0600, private parent directory, peer UID checked), one
//! newline-terminated JSON request and one newline-terminated JSON response
//! per connection, 256 KiB per frame, 10 s to read the request. The checks
//! are the local chat transport's (`local_phone_transport.rs`).
use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Map, Value};
use std::{
    fs::{File, OpenOptions},
    os::unix::{
        fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        io::AsRawFd,
    },
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};
use tokio_util::sync::CancellationToken;

pub const MAX_FRAME: usize = 256 * 1024;
pub const MAX_SOCKET_PATH: usize = 100;
const PROTOCOL_VERSION: u64 = 1;
const READ_TIMEOUT: Duration = Duration::from_secs(10);
const WORKERS: usize = 16;

/// A refusal sent as `{"ok":false,"code","error"}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpError {
    pub code: &'static str,
    pub error: String,
}

impl OpError {
    pub fn new(code: &'static str, error: impl Into<String>) -> Self {
        Self {
            code,
            error: error.into(),
        }
    }
}

/// One op: `op` is the request's `op`, `args` the rest of the object.
/// Returns the success fields (an object) without `ok`.
#[async_trait]
pub trait WebOps: Send + Sync {
    async fn dispatch(&self, op: &str, args: Value) -> Result<Value, OpError>;
}

/// `AUGMENTAGENT_WEB_SOCKET` must be absolute and at most 100 bytes.
pub fn socket_path(value: &std::ffi::OsStr) -> Result<PathBuf> {
    let path = PathBuf::from(value);
    ensure!(
        path.is_absolute(),
        "AUGMENTAGENT_WEB_SOCKET must be absolute"
    );
    ensure!(
        value.len() <= MAX_SOCKET_PATH,
        "AUGMENTAGENT_WEB_SOCKET must be at most 100 bytes"
    );
    Ok(path)
}

fn uid() -> u32 {
    unsafe { libc::geteuid() }
}

fn private_owned(metadata: &std::fs::Metadata) -> bool {
    metadata.uid() == uid() && metadata.mode() & 0o077 == 0
}

pub struct Listener {
    listener: UnixListener,
    path: PathBuf,
    inode: u64,
    peer_uid: u32,
    _lock: File,
}

impl Listener {
    pub fn bind(path: &Path) -> Result<Self> {
        let parent = path.parent().context("web socket needs a directory")?;
        let metadata = std::fs::symlink_metadata(parent)?;
        ensure!(
            metadata.is_dir() && private_owned(&metadata),
            "web socket directory must be private and owned by the daemon user"
        );
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path.with_extension("lock"))?;
        let metadata = lock.metadata()?;
        ensure!(
            metadata.is_file() && metadata.nlink() == 1 && private_owned(&metadata),
            "invalid web socket lock"
        );
        ensure!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "web socket already in use"
        );
        match std::fs::symlink_metadata(path) {
            Ok(metadata) => {
                ensure!(
                    metadata.file_type().is_socket() && metadata.uid() == uid(),
                    "refusing to replace non-socket web socket path"
                );
                // Only the lock holder can remove a crashed daemon's socket.
                std::fs::remove_file(path)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let listener = UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        let inode = std::fs::symlink_metadata(path)?.ino();
        Ok(Self {
            listener,
            path: path.into(),
            inode,
            peer_uid: uid(),
            _lock: lock,
        })
    }

    /// Test hook: accept only peers with this UID.
    #[cfg(test)]
    pub fn with_peer_uid(mut self, peer_uid: u32) -> Self {
        self.peer_uid = peer_uid;
        self
    }

    pub async fn serve(self, ops: Arc<dyn WebOps>, stop: CancellationToken) -> Result<()> {
        let slots = Arc::new(tokio::sync::Semaphore::new(WORKERS));
        let mut workers = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                _ = stop.cancelled() => break,
                Some(_) = workers.join_next(), if !workers.is_empty() => {},
                accepted = self.listener.accept() => {
                    let (stream, _) = accepted?;
                    if !stream.peer_cred().is_ok_and(|c| c.uid() == self.peer_uid) { continue; }
                    let Ok(permit) = slots.clone().try_acquire_owned() else { continue };
                    let ops = ops.clone();
                    workers.spawn(async move {
                        let _permit = permit;
                        // Message text and handler errors are not logged here.
                        let _ = connection(stream, ops.as_ref()).await;
                    });
                }
            }
        }
        // Turns run on their own tasks, so dropping a connection never
        // stops one (§5.2.1).
        workers.abort_all();
        while workers.join_next().await.is_some() {}
        Ok(())
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        if std::fs::symlink_metadata(&self.path)
            .is_ok_and(|m| m.file_type().is_socket() && m.ino() == self.inode && m.uid() == uid())
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn failure(code: &str, error: &str) -> Value {
    json!({"ok": false, "code": code, "error": error})
}

/// Parse one request frame and run it. Never fails: every problem is a
/// coded response.
pub async fn respond(frame: &[u8], ops: &dyn WebOps) -> Value {
    if frame.len() > MAX_FRAME {
        return failure("text_too_large", "request frame exceeds 256 KiB");
    }
    let Ok(Value::Object(mut request)) = serde_json::from_slice::<Value>(frame) else {
        return failure("bad_request", "request is not a JSON object");
    };
    if request.remove("v").and_then(|v| v.as_u64()) != Some(PROTOCOL_VERSION) {
        return failure("unsupported_version", "unsupported protocol version");
    }
    let Some(Value::String(op)) = request.remove("op") else {
        return failure("bad_request", "missing op");
    };
    match ops.dispatch(&op, Value::Object(request)).await {
        Ok(Value::Object(fields)) => {
            let mut body = Map::with_capacity(fields.len() + 1);
            body.insert("ok".into(), Value::Bool(true));
            body.extend(fields);
            Value::Object(body)
        }
        Ok(_) => failure("internal", "op returned a non-object result"),
        Err(error) => failure(error.code, &error.error),
    }
}

async fn connection(mut stream: UnixStream, ops: &dyn WebOps) -> Result<()> {
    let mut frame = Vec::new();
    let read = {
        let mut reader = BufReader::new(&mut stream).take(MAX_FRAME as u64 + 1);
        tokio::time::timeout(READ_TIMEOUT, reader.read_until(b'\n', &mut frame)).await
    };
    let response = match read {
        Ok(Ok(_)) if frame.len() > MAX_FRAME => {
            failure("text_too_large", "request frame exceeds 256 KiB")
        }
        Ok(Ok(_)) if frame.last() == Some(&b'\n') => {
            frame.pop();
            respond(&frame, ops).await
        }
        Ok(_) => failure(
            "bad_request",
            "request must be one newline-terminated frame",
        ),
        Err(_) => failure("bad_request", "request not received in time"),
    };
    let mut bytes = serde_json::to_vec(&response)?;
    bytes.push(b'\n');
    tokio::time::timeout(READ_TIMEOUT, stream.write_all(&bytes)).await??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;

    #[async_trait]
    impl WebOps for Echo {
        async fn dispatch(&self, op: &str, args: Value) -> Result<Value, OpError> {
            match op {
                "echo" => Ok(json!({"args": args})),
                _ => Err(OpError::new("bad_request", "unknown op")),
            }
        }
    }

    fn private_dir() -> tempfile::TempDir {
        // Unix socket paths must stay short even under a long TMPDIR.
        let dir = tempfile::Builder::new()
            .prefix("web-")
            .tempdir_in(std::fs::canonicalize("/tmp").unwrap())
            .unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    async fn exchange(path: &Path, frame: &[u8]) -> Option<Value> {
        let mut stream = UnixStream::connect(path).await.unwrap();
        stream.write_all(frame).await.unwrap();
        let mut out = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut out)).await;
        (!out.is_empty()).then(|| serde_json::from_slice(&out).unwrap())
    }

    fn serve(listener: Listener) -> CancellationToken {
        let stop = CancellationToken::new();
        tokio::spawn(listener.serve(Arc::new(Echo), stop.clone()));
        stop
    }

    #[tokio::test]
    async fn web_socket_frames_versions_ops_and_peer_uid() {
        let dir = private_dir();
        let path = dir.path().join("web.sock");
        let stop = serve(Listener::bind(&path).unwrap());
        let mode = std::fs::symlink_metadata(&path).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o600);

        let ok = exchange(&path, b"{\"v\":1,\"op\":\"echo\",\"x\":true}\n")
            .await
            .unwrap();
        assert_eq!(ok, json!({"ok": true, "args": {"x": true}}));
        let code = |v: Value| v["code"].as_str().unwrap().to_string();
        let bad = exchange(&path, b"{\"v\":2,\"op\":\"echo\"}\n")
            .await
            .unwrap();
        assert_eq!(code(bad), "unsupported_version");
        let unknown = exchange(&path, b"{\"v\":1,\"op\":\"nope\"}\n")
            .await
            .unwrap();
        assert_eq!(code(unknown), "bad_request");
        assert_eq!(
            code(exchange(&path, b"not json\n").await.unwrap()),
            "bad_request"
        );
        let mut big = br#"{"v":1,"op":"echo","text":""#.to_vec();
        big.extend(std::iter::repeat(b'x').take(MAX_FRAME));
        big.extend(b"\"}\n");
        assert_eq!(code(exchange(&path, &big).await.unwrap()), "text_too_large");
        // Exactly one request per connection.
        let first = exchange(
            &path,
            b"{\"v\":1,\"op\":\"echo\"}\n{\"v\":1,\"op\":\"echo\"}\n",
        )
        .await;
        assert_eq!(first.unwrap()["ok"], true);
        stop.cancel();
    }

    #[tokio::test]
    async fn web_socket_drops_other_uids_and_refuses_shared_directories() {
        let dir = private_dir();
        let path = dir.path().join("web.sock");
        let stop = serve(Listener::bind(&path).unwrap().with_peer_uid(uid() + 1));
        assert_eq!(exchange(&path, b"{\"v\":1,\"op\":\"echo\"}\n").await, None);
        stop.cancel();

        let shared = private_dir();
        std::fs::set_permissions(shared.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(Listener::bind(&shared.path().join("web.sock")).is_err());
        let plain = shared.path().join("file.sock");
        std::fs::set_permissions(shared.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(&plain, b"").unwrap();
        assert!(
            Listener::bind(&plain).is_err(),
            "never replaces a non-socket"
        );

        assert!(socket_path("relative.sock".as_ref()).is_err());
        assert!(socket_path(format!("/{}", "a".repeat(100)).as_ref()).is_err());
        assert!(socket_path("/run/web.sock".as_ref()).is_ok());
    }
}
