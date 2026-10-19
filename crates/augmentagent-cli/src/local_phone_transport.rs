//! Private local-chat transport. Identity comes from OS credentials, never JSON.
use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
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

const MAX_FRAME: u64 = 64 * 1024;
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u32,
    pub request_id: String,
    pub text: String,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub ok: bool,
    pub text: String,
}
#[async_trait]
pub trait Handler: Send + Sync {
    async fn handle(&self, request: Request) -> Result<String>;
}

pub struct Listener {
    listener: UnixListener,
    path: PathBuf,
    inode: u64,
    _lock: File,
}
fn uid() -> u32 {
    unsafe { libc::geteuid() }
}
fn private_owned(metadata: &std::fs::Metadata) -> bool {
    metadata.uid() == uid() && metadata.mode() & 0o077 == 0
}
impl Listener {
    pub fn bind(path: &Path) -> Result<Self> {
        let parent = path
            .parent()
            .context("local chat socket needs a directory")?;
        let metadata = std::fs::symlink_metadata(parent)?;
        ensure!(
            metadata.is_dir() && private_owned(&metadata),
            "local chat directory must be private and owned by the daemon user"
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
            "invalid local chat lock"
        );
        ensure!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "local chat already running"
        );
        match std::fs::symlink_metadata(path) {
            Ok(metadata) => {
                ensure!(
                    metadata.file_type().is_socket() && metadata.uid() == uid(),
                    "refusing to replace non-socket local chat path"
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
            _lock: lock,
        })
    }
    pub async fn serve(self, handler: Arc<dyn Handler>, stop: CancellationToken) -> Result<()> {
        let slots = Arc::new(tokio::sync::Semaphore::new(8));
        let mut workers = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                _ = stop.cancelled() => break,
                Some(_) = workers.join_next(), if !workers.is_empty() => {},
                accepted = self.listener.accept() => {
                    let (stream, _) = accepted?;
                    if !stream.peer_cred().is_ok_and(|credential| credential.uid() == uid()) { continue; }
                    let Ok(permit) = slots.clone().try_acquire_owned() else { continue };
                    let handler = handler.clone();
                    workers.spawn(async move {
                        let _permit = permit;
                        // No prompt, code, number or handler error is logged here.
                        let _ = connection(stream, handler.as_ref()).await;
                    });
                }
            }
        }
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
async fn frame(stream: &mut UnixStream) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut reader = BufReader::new(stream).take(MAX_FRAME + 1);
    tokio::time::timeout(
        Duration::from_secs(10),
        reader.read_until(b'\n', &mut bytes),
    )
    .await??;
    ensure!(
        bytes.len() <= MAX_FRAME as usize && bytes.last() == Some(&b'\n'),
        "invalid local chat frame"
    );
    Ok(bytes)
}
async fn write_frame(stream: &mut UnixStream, response: &Response) -> Result<()> {
    let mut response = response.clone();
    let mut bytes = serde_json::to_vec(&response)?;
    // JSON escaping can expand text by six bytes per control character. Bound
    // the serialized frame, preserving a valid success response after effects.
    while bytes.len() >= MAX_FRAME as usize {
        let mut end = response.text.len() / 2;
        while !response.text.is_char_boundary(end) {
            end -= 1;
        }
        response.text.truncate(end);
        response.text.push_str("\n[Response truncated.]");
        bytes = serde_json::to_vec(&response)?;
    }
    bytes.push(b'\n');
    tokio::time::timeout(Duration::from_secs(10), stream.write_all(&bytes)).await??;
    Ok(())
}
async fn connection(mut stream: UnixStream, handler: &dyn Handler) -> Result<()> {
    let request: Request = serde_json::from_slice(&frame(&mut stream).await?)?;
    ensure!(
        request.version == 1
            && uuid::Uuid::parse_str(&request.request_id).is_ok()
            && !request.text.trim().is_empty(),
        "invalid local chat request"
    );
    let response = match handler.handle(request).await {
        Ok(text) => Response { ok: true, text },
        _ => Response {
            ok: false,
            text: "Local conversation could not complete. Check status before retrying an action."
                .into(),
        },
    };
    write_frame(&mut stream, &response).await
}

/// Send one private request. The caller retains its request ID when outcome is
/// uncertain; this transport never retries an action automatically.
pub async fn request(path: &Path, request: &Request) -> Result<Response> {
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        metadata.file_type().is_socket() && private_owned(&metadata),
        "invalid local chat socket"
    );
    let mut stream = UnixStream::connect(path).await?;
    ensure!(
        stream.peer_cred()?.uid() == uid(),
        "local chat daemon belongs to another user"
    );
    let mut bytes = serde_json::to_vec(request)?;
    ensure!(
        bytes.len() < MAX_FRAME as usize,
        "local chat request too large"
    );
    bytes.push(b'\n');
    stream.write_all(&bytes).await?;
    // Model execution may outlive the framing timeout. Wait for the first byte
    // separately, then bound completion of the response frame.
    let mut first = [0u8; 1];
    tokio::time::timeout(Duration::from_secs(900), stream.read_exact(&mut first)).await??;
    let mut response = first.to_vec();
    response.extend(frame(&mut stream).await?);
    ensure!(
        response.len() <= MAX_FRAME as usize,
        "local chat response too large"
    );
    Ok(serde_json::from_slice(&response)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[derive(Default)]
    struct Echo(AtomicUsize);
    #[async_trait]
    impl Handler for Echo {
        async fn handle(&self, request: Request) -> Result<String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(request.text)
        }
    }
    fn directory() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }
    #[tokio::test]
    async fn socket_is_private_and_duplicate_listener_does_not_unlink_it() {
        let dir = directory();
        let path = dir.path().join("chat.sock");
        let listener = Listener::bind(&path).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert!(Listener::bind(&path).is_err());
        let echo = Arc::new(Echo::default());
        let stop = CancellationToken::new();
        let task = tokio::spawn(listener.serve(echo.clone(), stop.clone()));
        let input = Request {
            version: 1,
            request_id: uuid::Uuid::new_v4().to_string(),
            text: "synthetic prompt".into(),
        };
        let response = request(&path, &input).await.unwrap();
        assert!(response.ok);
        assert_eq!(response.text, "synthetic prompt");
        assert_eq!(echo.0.load(Ordering::SeqCst), 1);
        stop.cancel();
        task.await.unwrap().unwrap();
        assert!(!path.exists());
    }
    #[tokio::test]
    async fn unsafe_paths_are_rejected_and_stale_socket_is_recovered() {
        let dir = directory();
        let path = dir.path().join("chat.sock");
        std::fs::write(&path, "preserve").unwrap();
        assert!(Listener::bind(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "preserve");
        std::fs::remove_file(&path).unwrap();
        let stale = UnixListener::bind(&path).unwrap();
        drop(stale);
        let listener = Listener::bind(&path).unwrap();
        drop(listener);
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(Listener::bind(&path).is_err());
    }
    #[tokio::test]
    async fn symlink_lock_and_replaced_socket_are_not_followed_or_removed() {
        let dir = directory();
        let path = dir.path().join("chat.sock");
        let target = dir.path().join("target");
        std::fs::write(&target, "retain").unwrap();
        std::os::unix::fs::symlink(&target, path.with_extension("lock")).unwrap();
        assert!(Listener::bind(&path).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "retain");
        std::fs::remove_file(path.with_extension("lock")).unwrap();
        let listener = Listener::bind(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, "replacement").unwrap();
        drop(listener);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "replacement");
    }
    #[tokio::test]
    async fn invalid_or_impersonating_frames_never_reach_handler() {
        let dir = directory();
        let path = dir.path().join("chat.sock");
        let listener = Listener::bind(&path).unwrap();
        let echo = Arc::new(Echo::default());
        let stop = CancellationToken::new();
        let task = tokio::spawn(listener.serve(echo.clone(), stop.clone()));
        for body in [
            format!(
                "{{\"version\":1,\"request_id\":\"{}\",\"text\":\"hello\",\"owner\":\"forged\"}}\n",
                uuid::Uuid::new_v4()
            ),
            "{\"version\":1,\"request_id\":\"bad\",\"text\":\"hello\"}\n".into(),
            "x".repeat(MAX_FRAME as usize + 1),
        ] {
            let mut stream = UnixStream::connect(&path).await.unwrap();
            let _ = stream.write_all(body.as_bytes()).await;
            stream.shutdown().await.unwrap();
            let mut response = Vec::new();
            let _ = tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut response))
                .await
                .unwrap();
            assert!(response.is_empty());
        }
        assert_eq!(echo.0.load(Ordering::SeqCst), 0);
        stop.cancel();
        task.await.unwrap().unwrap();
    }
    #[tokio::test]
    async fn escaped_large_reply_is_bounded_without_losing_completed_result() {
        struct Escaped;
        #[async_trait]
        impl Handler for Escaped {
            async fn handle(&self, _: Request) -> Result<String> {
                Ok("\u{0001}Ω".repeat(10000))
            }
        }
        let dir = directory();
        let path = dir.path().join("chat.sock");
        let listener = Listener::bind(&path).unwrap();
        let stop = CancellationToken::new();
        let task = tokio::spawn(listener.serve(Arc::new(Escaped), stop.clone()));
        let input = Request {
            version: 1,
            request_id: uuid::Uuid::new_v4().to_string(),
            text: "synthetic".into(),
        };
        let result = request(&path, &input).await;
        stop.cancel();
        task.await.unwrap().unwrap();
        let response = result.expect("completed result must remain a valid protocol reply");
        assert!(response.ok);
        assert!(response.text.starts_with("\u{0001}Ω"));
        assert!(response.text.ends_with("[Response truncated.]"));
        assert!(serde_json::to_vec(&response).unwrap().len() < MAX_FRAME as usize);
    }
}
