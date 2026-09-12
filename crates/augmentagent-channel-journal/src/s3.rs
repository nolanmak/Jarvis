//! #888 — allowlisted, size-capped download of one iMessage attachment: the
//! `s3://<bucket>/conversations/<dir>/attachments/<id>-<name>` pointer on a
//! bundle `[attachment: …]` line, fetched by `augmentagent imessage
//! fetch-attachment` into the ask session's own dir under
//! [`ATTACHMENT_TMP_ROOT`]. Enforced here: the bucket/prefix allowlist
//! (pre-network), the streamed [`MAX_ATTACHMENT_BYTES`] cap, private
//! (verified) temp dirs, and per-session cleanup.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use aws_credential_types::provider::{ProvideCredentials, SharedCredentialsProvider};
use aws_sigv4::http_request::{
    sign, PayloadChecksumKind, PercentEncodingMode, SignableBody, SignableRequest,
    SigningSettings, UriPathNormalizationMode,
};
use aws_sigv4::sign::v4;
use md5::{Digest, Md5};
use thiserror::Error;
use tokio::io::AsyncWriteExt;

/// One `<pid>-<nanos>` dir per ask session (`ask_opts` mints it as
/// `$AUGMENTAGENT_IMESSAGE_TMP_DIR`; the scope guard admits Reads only there).
pub const ATTACHMENT_TMP_ROOT: &str = "/tmp/aa-imsg";
pub const SESSION_DIR_ENV: &str = "AUGMENTAGENT_IMESSAGE_TMP_DIR";
pub const MAX_ATTACHMENT_BYTES: u64 = 25 * 1024 * 1024;
/// Session dirs untouched this long are crash leftovers: a live ask session is
/// bounded by the reasoner watchdog (2 h default), so a day-old dir is nobody's.
pub const GC_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// Key prefix the exporter writes under (`scripts/imessage/imessage_sync.py`).
pub const DEFAULT_PREFIX: &str = "conversations/";

#[derive(Debug, Error)]
pub enum S3FetchError {
    #[error("iMessage attachment fetch is not configured: set AUGMENTAGENT_IMESSAGE_S3_BUCKET")]
    NotConfigured,
    #[error("refused: {0}")]
    Refused(String),
    #[error("signing: {0}")]
    Sign(String),
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("s3 returned {status}: {body}")]
    Status { status: u16, body: String },
    #[error("attachment exceeds the {MAX_ATTACHMENT_BYTES}-byte cap")]
    TooLarge,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// The one bucket/prefix an ask-mode fetch may touch.
#[derive(Debug, Clone)]
pub struct AttachmentSource {
    bucket: String,
    /// Always ends in `/`.
    prefix: String,
    region: String,
    /// Path-style base URL for tests; virtual-hosted S3 when `None`.
    endpoint: Option<String>,
    http: reqwest::Client,
}

/// S3's canonical key encoding: RFC 3986 unreserved plus `/` survive, the rest
/// is `%XX` (conversation dirs contain spaces, `+` and `@`).
fn encode_key(key: &str) -> String {
    key.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => (b as char).into(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Local file name for `key`: basename sanitized to `[A-Za-z0-9._-]`, a short
/// hash of the full key before the extension (look-alikes never collide), the
/// extension the Read tool keys off intact.
pub fn local_name(key: &str) -> String {
    let base = key.rsplit('/').next().unwrap_or(key);
    let safe: String = base
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' })
        .collect();
    let hash = &format!("{:x}", Md5::digest(key.as_bytes()))[..8];
    match safe.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => format!("{stem}-{hash}.{ext}"),
        _ => format!("{safe}-{hash}"),
    }
}

/// `$AUGMENTAGENT_IMESSAGE_TMP_DIR` when an ask session minted one (accepted only
/// as a direct child of the root, so a stray value cannot steer writes elsewhere), else per-pid.
pub fn session_dir() -> PathBuf {
    let root = Path::new(ATTACHMENT_TMP_ROOT);
    std::env::var_os(SESSION_DIR_ENV)
        .map(PathBuf::from)
        .filter(|d| d.parent() == Some(root))
        .unwrap_or_else(|| root.join(std::process::id().to_string()))
}

/// Create `dir` (`<root>/<session>`): each level a non-recursive 0700 mkdir,
/// then lstat-verified as a real directory (never a symlink) with no
/// group/other bits. `/tmp` is sticky, so a pre-planted root is the one way
/// another user could swap the session dir for a link under a download
/// (CWE-59): a foreign 0700 root fails the mkdir inside it, a looser one fails
/// here. No uid check — the workspace keeps `libc` out; bits cover all but ACLs.
pub fn prepare_tmp_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    let root = dir.parent().ok_or_else(|| std::io::Error::other("session dir has no parent"))?;
    for d in [root, dir] {
        match std::fs::DirBuilder::new().mode(0o700).create(d) {
            Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(e),
            _ => {}
        }
        let meta = std::fs::symlink_metadata(d)?;
        if !meta.is_dir() || meta.mode() & 0o077 != 0 {
            let why = "not a private directory (symlink, or group/other access)";
            return Err(std::io::Error::other(format!("{}: {why}", d.display())));
        }
    }
    Ok(())
}

/// Delete one session's dir and everything in it (symlinks inside are removed,
/// never followed) — only that dir. Returns whether anything was there.
pub fn remove_session_dir(dir: &Path) -> bool {
    std::fs::remove_dir_all(dir).is_ok()
}

/// Delete the session dirs directly under `root` with an mtime at least
/// `max_age` old — what a daemon killed mid-ask leaves behind. Top-level
/// files/symlinks are skipped (`DirEntry::metadata` is an lstat).
pub fn sweep_stale_sessions(root: &Path, max_age: Duration) -> usize {
    let Ok(entries) = std::fs::read_dir(root) else { return 0 };
    let now = SystemTime::now();
    let stale = |m: std::fs::Metadata| {
        m.is_dir() && m.modified().ok().and_then(|t| now.duration_since(t).ok()).is_some_and(|a| a >= max_age)
    };
    entries
        .flatten()
        .filter(|e| e.metadata().is_ok_and(&stale) && remove_session_dir(&e.path()))
        .count()
}

impl AttachmentSource {
    pub fn new(bucket: &str, prefix: Option<&str>, region: &str) -> Self {
        let p = prefix.map(str::trim).filter(|p| !p.is_empty()).unwrap_or(DEFAULT_PREFIX);
        Self {
            bucket: bucket.to_string(),
            prefix: format!("{}/", p.trim_matches('/')),
            region: region.to_string(),
            endpoint: None,
            http: reqwest::Client::new(),
        }
    }

    /// `AUGMENTAGENT_IMESSAGE_S3_BUCKET` / `_PREFIX` / `_ENDPOINT` (path-style
    /// base URL for S3-compatible or mock servers) and `AWS_REGION` (or
    /// `AWS_DEFAULT_REGION`, else `us-east-1` — S3 rejects a wrong one loudly).
    pub fn from_env() -> Result<Self, S3FetchError> {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let bucket = var("AUGMENTAGENT_IMESSAGE_S3_BUCKET").ok_or(S3FetchError::NotConfigured)?;
        let region = var("AWS_REGION")
            .or_else(|| var("AWS_DEFAULT_REGION"))
            .unwrap_or_else(|| "us-east-1".into());
        let mut src = Self::new(bucket.trim(), var("AUGMENTAGENT_IMESSAGE_S3_PREFIX").as_deref(), &region);
        src.endpoint = var("AUGMENTAGENT_IMESSAGE_S3_ENDPOINT").map(|u| u.trim_end_matches('/').into());
        Ok(src)
    }

    /// The allowlist, pre-network: configured bucket + prefix, no `..` segment, non-empty basename.
    pub fn resolve_key(&self, uri: &str) -> Result<String, S3FetchError> {
        let refuse = |why: String| Err(S3FetchError::Refused(format!("{why}: {uri}")));
        let Some((bucket, key)) = uri.strip_prefix("s3://").and_then(|r| r.split_once('/')) else {
            return refuse("not an s3://bucket/key URI".into());
        };
        if bucket != self.bucket {
            return refuse(format!("bucket is not the configured `{}`", self.bucket));
        }
        if !key.starts_with(&self.prefix) {
            return refuse(format!("key is outside the allowed prefix `{}`", self.prefix));
        }
        if key.ends_with('/') || key.split('/').any(|seg| seg == "..") {
            return refuse("malformed key".into());
        }
        Ok(key.to_string())
    }

    fn object_url(&self, key: &str) -> String {
        match &self.endpoint {
            Some(base) => format!("{base}/{}/{}", self.bucket, encode_key(key)),
            None => format!("https://{}.s3.{}.amazonaws.com/{}", self.bucket, self.region, encode_key(key)),
        }
    }

    async fn signed_get(
        &self,
        credentials: &SharedCredentialsProvider,
        url: &str,
    ) -> Result<reqwest::Request, S3FetchError> {
        let sign_err = |e: &dyn std::fmt::Display| S3FetchError::Sign(e.to_string());
        let creds = credentials.provide_credentials().await.map_err(|e| sign_err(&e))?;
        let identity = creds.into();
        // S3 signs the single-encoded path verbatim; wants the payload hash header even if unsigned.
        let mut settings = SigningSettings::default();
        settings.percent_encoding_mode = PercentEncodingMode::Single;
        settings.uri_path_normalization_mode = UriPathNormalizationMode::Disabled;
        settings.payload_checksum_kind = PayloadChecksumKind::XAmzSha256;
        let params: aws_sigv4::http_request::SigningParams = v4::SigningParams::builder()
            .identity(&identity)
            .region(&self.region)
            .name("s3")
            .time(SystemTime::now())
            .settings(settings)
            .build()
            .map_err(|e| sign_err(&e))?
            .into();
        let uri: http::Uri = url.parse().map_err(|e| sign_err(&e))?;
        let host = uri.authority().ok_or_else(|| S3FetchError::Sign("URL has no host".into()))?;
        let headers = [("host", host.as_str())].into_iter();
        let signable = SignableRequest::new("GET", url, headers, SignableBody::UnsignedPayload)
            .map_err(|e| sign_err(&e))?;
        let (instructions, _) = sign(signable, &params).map_err(|e| sign_err(&e))?.into_parts();
        let mut request = http::Request::get(url).body(String::new()).map_err(|e| sign_err(&e))?;
        instructions.apply_to_request_http1x(&mut request);
        Ok(reqwest::Request::try_from(request)?)
    }

    /// Production fetch: default SDK credential chain, loaded only after the allowlist passes.
    pub async fn fetch(&self, uri: &str, dest: &Path) -> Result<u64, S3FetchError> {
        self.resolve_key(uri)?;
        let sdk = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new(self.region.clone()))
            .load()
            .await;
        let creds = sdk
            .credentials_provider()
            .ok_or_else(|| S3FetchError::Sign("no AWS credentials provider".into()))?;
        self.fetch_with(&creds, uri, dest).await
    }

    /// Download `uri` to `dest`, streaming under [`MAX_ATTACHMENT_BYTES`]; any
    /// failure after the file is opened removes the partial file. Returns bytes written.
    pub async fn fetch_with(
        &self,
        credentials: &SharedCredentialsProvider,
        uri: &str,
        dest: &Path,
    ) -> Result<u64, S3FetchError> {
        let key = self.resolve_key(uri)?;
        let request = self.signed_get(credentials, &self.object_url(&key)).await?;
        let mut resp = self.http.execute(request).await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default().chars().take(300).collect();
            return Err(S3FetchError::Status { status: status.as_u16(), body });
        }
        if resp.content_length().is_some_and(|len| len > MAX_ATTACHMENT_BYTES) {
            return Err(S3FetchError::TooLarge);
        }
        // Unlink `dest` as a path entry, then O_EXCL: a link is never written through.
        let _ = tokio::fs::remove_file(dest).await;
        let mut file = tokio::fs::OpenOptions::new().write(true).create_new(true).open(dest).await?;
        let mut written: u64 = 0;
        let streamed: Result<(), S3FetchError> = async {
            while let Some(chunk) = resp.chunk().await? {
                written += chunk.len() as u64;
                if written > MAX_ATTACHMENT_BYTES {
                    return Err(S3FetchError::TooLarge);
                }
                file.write_all(&chunk).await?;
            }
            Ok(file.flush().await?)
        }
        .await;
        drop(file);
        if let Err(e) = streamed {
            let _ = tokio::fs::remove_file(dest).await;
            return Err(e);
        }
        Ok(written)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_credential_types::Credentials;

    const URI: &str = "s3://imsg-bundle/conversations/Alice B/attachments/9-IMG_001.jpeg";

    fn creds() -> SharedCredentialsProvider {
        SharedCredentialsProvider::new(Credentials::new("AKIDTEST", "SECRETTEST", None, None, "t"))
    }

    fn source(endpoint: &str) -> AttachmentSource {
        let mut src = AttachmentSource::new("imsg-bundle", None, "us-east-1");
        src.endpoint = Some(endpoint.into());
        src
    }

    #[tokio::test]
    async fn wrong_bucket_or_prefix_is_refused_before_any_request() {
        let mut server = mockito::Server::new_async().await;
        let mock = server.mock("GET", mockito::Matcher::Any).expect(0).create_async().await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("out");
        for uri in [
            "s3://other-bucket/conversations/Alice B/attachments/9-IMG_001.jpeg",
            "s3://imsg-bundle/backups/attachments/9-IMG_001.jpeg",
            "s3://imsg-bundle/conversations/../secrets/x",
            "https://imsg-bundle.s3.amazonaws.com/conversations/x",
        ] {
            let err = source(&server.url()).fetch_with(&creds(), uri, &dest).await.unwrap_err();
            assert!(matches!(err, S3FetchError::Refused(_)), "{uri}: {err}");
        }
        mock.assert_async().await;
        assert!(!dest.exists());
    }

    #[tokio::test]
    async fn signed_get_lands_bytes_at_dest() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            // Spaces in the conversation dir must reach the wire encoded.
            .mock("GET", "/imsg-bundle/conversations/Alice%20B/attachments/9-IMG_001.jpeg")
            .match_header("authorization", mockito::Matcher::Regex("^AWS4-HMAC-SHA256 .*s3.*".into()))
            .match_header("x-amz-date", mockito::Matcher::Any)
            .match_header("x-amz-content-sha256", "UNSIGNED-PAYLOAD")
            .with_body(b"jpegbytes")
            .create_async()
            .await;
        // The local name must pass the guard's `[A-Za-z0-9._-]+` and keep look-alike keys apart.
        let name = local_name("conversations/Alice B/attachments/9-IMG 001.jpeg");
        assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)), "{name}");
        assert!(name.starts_with("9-IMG_001-") && name.ends_with(".jpeg"), "{name}");
        assert_ne!(name, local_name("conversations/Bob C/attachments/9-IMG 001.jpeg"));
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join(name);
        let n = source(&server.url()).fetch_with(&creds(), URI, &dest).await.unwrap();
        mock.assert_async().await;
        assert_eq!(n, 9);
        assert_eq!(std::fs::read(&dest).unwrap(), b"jpegbytes");
    }

    #[tokio::test]
    async fn oversized_body_is_aborted_and_partial_file_removed() {
        let mut server = mockito::Server::new_async().await;
        let body = vec![b'x'; MAX_ATTACHMENT_BYTES as usize + 1];
        // Chunked: no Content-Length shortcut, the streamed counter must trip.
        server
            .mock("GET", mockito::Matcher::Any)
            .with_chunked_body(move |w| w.write_all(&body))
            .create_async()
            .await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("big.mov");
        let err = source(&server.url()).fetch_with(&creds(), URI, &dest).await.unwrap_err();
        assert!(matches!(err, S3FetchError::TooLarge), "{err}");
        assert!(!dest.exists(), "partial file must be deleted");
    }

    /// Codex reviews — a root another user pre-planted in sticky `/tmp` (a
    /// symlink, or a dir they could rename our session dir out of) and a
    /// session dir swapped for a link are refused before anything is written;
    /// our own private dirs are reusable. Ending one session removes only its
    /// dir; the sweep reclaims only day-old (crashed) siblings.
    #[test]
    fn hostile_tmp_entries_are_refused_and_cleanup_is_per_session() {
        use std::fs::{set_permissions, Permissions};
        use std::os::unix::fs::{symlink, PermissionsExt};
        let tmp = tempfile::tempdir().unwrap();
        let (root, elsewhere) = (tmp.path().join("aa-imsg"), tmp.path().join("elsewhere"));
        std::fs::create_dir(&elsewhere).unwrap();
        symlink(&elsewhere, &root).unwrap();
        assert!(prepare_tmp_dir(&root.join("1-1")).is_err(), "root is a symlink");
        assert!(!elsewhere.join("1-1").exists(), "nothing created through the link");
        std::fs::remove_file(&root).unwrap();
        std::fs::create_dir(&root).unwrap();
        set_permissions(&root, Permissions::from_mode(0o777)).unwrap();
        assert!(prepare_tmp_dir(&root.join("1-1")).is_err(), "root writable by others");
        set_permissions(&root, Permissions::from_mode(0o700)).unwrap();
        symlink(&elsewhere, root.join("9-0")).unwrap();
        assert!(prepare_tmp_dir(&root.join("9-0")).is_err(), "session dir is a symlink");
        std::fs::remove_file(root.join("9-0")).unwrap();

        let (own, other, crashed) = (root.join("1-1"), root.join("1-2"), root.join("9-0"));
        for d in [&own, &other, &crashed] {
            prepare_tmp_dir(d).unwrap();
            prepare_tmp_dir(d).unwrap();
            assert_eq!(std::fs::metadata(d).unwrap().permissions().mode() & 0o777, 0o700);
            std::fs::write(d.join("a.jpeg"), b"x").unwrap();
        }
        std::fs::File::open(&crashed).unwrap().set_modified(SystemTime::now() - GC_MAX_AGE * 2).unwrap();

        assert!(remove_session_dir(&own));
        assert!(!remove_session_dir(&own), "already gone");
        assert_eq!(sweep_stale_sessions(&root, GC_MAX_AGE), 1);
        assert!(!own.exists() && !crashed.exists());
        assert!(other.join("a.jpeg").exists(), "the concurrent session keeps its file");
    }
}
