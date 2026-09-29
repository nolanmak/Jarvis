//! #1293 — files the owner sends in Slack, turned into the same turn input
//! Discord builds (`augmentagent_docs::inbound`): `IMAGE:` markers for
//! images, a path list for text files and for PDF/DOCX converted to text,
//! and an owner-facing "skipped" line for everything that was refused.
//!
//! [`prepare_inbound`] takes the message text and its [`FileRef`]s (from a
//! `message` event, including `file_share` messages with empty text) and:
//!
//! 1. refuses, before any download, files that are unsupported, credential
//!    formats, over the size cap Slack declared, deleted, external, Slack
//!    Connect files that need `files.info`, or past [`MAX_FILES_PER_MESSAGE`];
//! 2. streams the rest through [`SlackWebApi::download_file`] (bot token,
//!    Slack file hosts only, cap enforced while streaming) into a private
//!    per-message directory under [`default_inbound_root`];
//! 3. truncates long text, converts documents with the shared bounded
//!    converter (`augmentagent_docs::extract_text_with`), and builds the
//!    prompt with the shared `build_prompt`.
//!
//! Files live only as long as the returned [`InboundMessage`]: dropping it,
//! calling [`InboundMessage::cleanup`], a failure or a cancellation removes
//! the directory, matching Discord's per-turn cleanup until #995 defines
//! shared retention. The serve dispatcher (#1287/#1288) calls this for an
//! owner message; `augmentagent slack files fetch` is the operator path.

use std::path::{Path, PathBuf};

use augmentagent_docs::inbound::{
    build_prompt, classify, extension_for, format_rejection_footer, sanitize_filename, InboundKind,
    RejectReason, Rejected, TextAttachment, MAX_TEXT_BYTES,
};
use augmentagent_docs::{extract_text_with, ConvertOptions, OcrClient};
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::transport::event::FileRef;
use crate::transport::web::{DownloadRequest, SlackWebApi, WebApiError};

/// Directory name under the shared state dir.
pub const SLACK_INBOUND_DIR: &str = "slack-inbound";

/// Files handled per message; the rest are skipped with a reason. Discord
/// caps a message at 10 attachments, so this keeps parity.
pub const MAX_FILES_PER_MESSAGE: usize = 10;

/// `<state dir>/slack-inbound` (`$XDG_STATE_HOME/augmentagent` or
/// `~/.local/state/augmentagent`), the same private location on Linux and
/// macOS. `None` without `HOME`.
pub fn default_inbound_root() -> Option<PathBuf> {
    augmentagent_channel_core::state_dir::state_dir().map(|d| d.join(SLACK_INBOUND_DIR))
}

/// How [`prepare_inbound`] stores and converts files.
#[derive(Debug, Clone)]
pub struct InboundOptions {
    /// Parent of the per-message directories; created 0700.
    pub root: PathBuf,
    /// Converter lookup and timeout, shared with Discord's pipeline.
    pub convert: ConvertOptions,
    /// Mistral OCR for scanned PDFs; `None` = skipped with a note (#939).
    pub ocr: Option<OcrClient>,
    pub max_files: usize,
}

impl InboundOptions {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            convert: ConvertOptions::default(),
            ocr: None,
            max_files: MAX_FILES_PER_MESSAGE,
        }
    }
}

#[derive(Debug, Error)]
pub enum InboundError {
    /// The turn was cancelled; everything downloaded so far is removed.
    #[error("cancelled")]
    Cancelled,
    /// The private directory could not be created safely.
    #[error("inbound file storage: {0}")]
    Storage(String),
}

/// One file handed to the reasoner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedFile {
    pub file_id: String,
    /// Name as the owner sent it (display only; never used as a path).
    pub original_name: String,
    pub kind: InboundKind,
    /// Image, text file, or the extracted text of a document.
    pub path: PathBuf,
    /// Bytes downloaded from Slack.
    pub downloaded_bytes: u64,
}

/// The turn input built from one owner message.
#[derive(Debug)]
pub struct InboundMessage {
    /// The message text, trimmed (empty for an attachment-only message).
    pub user_text: String,
    /// What goes to the reasoner (Discord's `build_prompt`).
    pub prompt: String,
    pub images: Vec<PathBuf>,
    pub text_files: Vec<TextAttachment>,
    pub accepted: Vec<AcceptedFile>,
    pub rejected: Vec<Rejected>,
    dir: Option<tempfile::TempDir>,
}

impl InboundMessage {
    /// Whether the reasoner has anything to work with. A message whose files
    /// were all refused and that has no text only gets the notice.
    pub fn starts_turn(&self) -> bool {
        !self.user_text.is_empty() || !self.images.is_empty() || !self.text_files.is_empty()
    }

    /// Owner-facing "skipped: …" line, `None` when nothing was refused.
    pub fn rejection_notice(&self) -> Option<String> {
        format_rejection_footer(&self.rejected)
    }

    /// The private per-message directory, if any file was downloaded.
    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_ref().map(|d| d.path())
    }

    /// Remove the files now (dropping the message does the same, best
    /// effort).
    pub fn cleanup(mut self) -> std::io::Result<()> {
        match self.dir.take() {
            Some(dir) => dir.close(),
            None => Ok(()),
        }
    }
}

/// Create `root` (and parents) and require it to be a real directory owned
/// by us with no group/other access; tighten a fresh or loose one to 0700.
fn private_root(root: &Path) -> Result<(), InboundError> {
    let fail = |what: &str, e: &dyn std::fmt::Display| {
        InboundError::Storage(format!("{what} {}: {e}", root.display()))
    };
    std::fs::create_dir_all(root).map_err(|e| fail("cannot create", &e))?;
    let meta = std::fs::symlink_metadata(root).map_err(|e| fail("cannot inspect", &e))?;
    if !meta.is_dir() {
        return Err(InboundError::Storage(format!(
            "{} is not a directory (a symlink is refused)",
            root.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // SAFETY: geteuid has no preconditions and cannot fail.
        let uid = unsafe { libc::geteuid() };
        if meta.uid() != uid {
            return Err(InboundError::Storage(format!(
                "{} is owned by another user",
                root.display()
            )));
        }
        if meta.permissions().mode() & 0o077 != 0 {
            std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))
                .map_err(|e| fail("cannot restrict", &e))?;
        }
    }
    Ok(())
}

/// Why a file is not available to the app, before any download.
fn unavailable(f: &FileRef) -> Option<&'static str> {
    match f.mode.as_deref() {
        Some("tombstone") => return Some("the file was deleted"),
        Some("hidden_by_limit") => return Some("hidden by the workspace's plan limit"),
        _ => {}
    }
    if f.is_external || f.mode.as_deref() == Some("external") {
        return Some("the file is hosted outside Slack");
    }
    if f.file_access.as_deref() == Some("check_file_info") {
        return Some("Slack Connect file not readable by the app yet");
    }
    None
}

fn download_reason(e: &WebApiError) -> String {
    match e.root() {
        WebApiError::Timeout => "couldn't download: timed out".into(),
        WebApiError::RateLimited { retry_after } => format!(
            "couldn't download: Slack rate limit, retry in {}s",
            retry_after.as_secs().max(1)
        ),
        WebApiError::FileHostRefused(_) => {
            "couldn't download: the link is not a Slack file host".into()
        }
        WebApiError::DownloadRejected(_) => {
            "couldn't download: Slack refused access (check the files:read scope)".into()
        }
        WebApiError::Http { status, .. } => format!("couldn't download: HTTP {status}"),
        _ => "couldn't download the file".into(),
    }
}

/// Short, owner-facing form of a converter error (first line, no chain).
fn conversion_reason(e: &anyhow::Error) -> String {
    // The innermost cause is the converter's own message ("pdftotext is
    // not installed…", "timed out…"); outer contexts only add noise.
    let first = e.root_cause().to_string();
    let first = first.lines().next().unwrap_or("conversion failed");
    let mut short: String = first.chars().take(240).collect();
    if short.len() < first.len() {
        short.push('…');
    }
    format!("couldn't read the document: {short}")
}

/// Build the turn input for one owner message. See the module docs.
pub async fn prepare_inbound(
    api: &dyn SlackWebApi,
    text: &str,
    files: &[FileRef],
    opts: &InboundOptions,
    cancel: &CancellationToken,
) -> Result<InboundMessage, InboundError> {
    let user_text = text.trim().to_string();
    let mut msg = InboundMessage {
        user_text,
        prompt: String::new(),
        images: Vec::new(),
        text_files: Vec::new(),
        accepted: Vec::new(),
        rejected: Vec::new(),
        dir: None,
    };
    let reject = |msg: &mut InboundMessage, name: &str, reason: RejectReason| {
        msg.rejected.push(Rejected {
            filename: name.to_string(),
            reason,
        });
    };

    for (index, f) in files.iter().enumerate() {
        if cancel.is_cancelled() {
            return Err(InboundError::Cancelled);
        }
        let name = f
            .name
            .clone()
            .or_else(|| f.title.clone())
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| f.id.clone());
        if index >= opts.max_files {
            let why = format!("more than {} files per message", opts.max_files);
            reject(&mut msg, &name, RejectReason::Unavailable(why));
            continue;
        }
        if let Some(why) = unavailable(f) {
            reject(&mut msg, &name, RejectReason::Unavailable(why.into()));
            continue;
        }
        let declared = f.size.unwrap_or(0);
        let kind = match classify(&name, f.mimetype.as_deref(), declared) {
            Ok(kind) => kind,
            Err(reason) => {
                reject(&mut msg, &name, reason);
                continue;
            }
        };
        let cap = kind.download_cap();
        if declared > cap {
            reject(
                &mut msg,
                &name,
                RejectReason::Oversize {
                    size: declared,
                    limit: cap,
                },
            );
            continue;
        }
        let Some(url) = f
            .url_private_download
            .as_deref()
            .or(f.url_private.as_deref())
        else {
            let why = "Slack sent no download link".to_string();
            reject(&mut msg, &name, RejectReason::Unavailable(why));
            continue;
        };

        if msg.dir.is_none() {
            private_root(&opts.root)?;
            let mut builder = tempfile::Builder::new();
            builder.prefix("msg-");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                builder.permissions(std::fs::Permissions::from_mode(0o700));
            }
            let dir = builder
                .tempdir_in(&opts.root)
                .map_err(|e| InboundError::Storage(format!("cannot create message dir: {e}")))?;
            msg.dir = Some(dir);
        }
        let dir = msg
            .dir
            .as_ref()
            .expect("created above")
            .path()
            .to_path_buf();
        // Keep a usable extension (the codex image bridge and the converters
        // key off it) even when the name has none.
        let disk_name = if Path::new(&name).extension().is_some() {
            name.clone()
        } else {
            format!("{name}.{}", extension_for(&name, f.mimetype.as_deref()))
        };
        let dest = dir.join(sanitize_filename(index, &disk_name));
        let request = DownloadRequest {
            url,
            dest: &dest,
            max_bytes: cap,
            expected_mimetype: f.mimetype.as_deref(),
        };
        let downloaded = tokio::select! {
            _ = cancel.cancelled() => {
                let _ = std::fs::remove_file(&dest);
                return Err(InboundError::Cancelled);
            }
            r = api.download_file(request) => r,
        };
        let bytes = match downloaded {
            Ok(n) => n,
            Err(e) => {
                let _ = std::fs::remove_file(&dest);
                if matches!(e.root(), WebApiError::Cancelled) {
                    return Err(InboundError::Cancelled);
                }
                warn!(file_id = %f.id, error = %e, "slack inbound download failed");
                let reason = match e.root() {
                    WebApiError::FileTooLarge { size, limit } => RejectReason::Oversize {
                        size: *size,
                        limit: *limit,
                    },
                    other => RejectReason::Unavailable(download_reason(other)),
                };
                reject(&mut msg, &name, reason);
                continue;
            }
        };
        debug!(file_id = %f.id, bytes, ?kind, "slack inbound file downloaded");

        let accepted_path = match kind {
            InboundKind::Image => {
                msg.images.push(dest.clone());
                dest
            }
            InboundKind::Text => {
                let truncated = bytes > MAX_TEXT_BYTES;
                if truncated {
                    if let Err(e) = std::fs::OpenOptions::new()
                        .write(true)
                        .open(&dest)
                        .and_then(|f| f.set_len(MAX_TEXT_BYTES))
                    {
                        let _ = std::fs::remove_file(&dest);
                        let why = format!("couldn't store the file ({e})");
                        reject(&mut msg, &name, RejectReason::Unavailable(why));
                        continue;
                    }
                }
                msg.text_files.push(TextAttachment {
                    path: dest.clone(),
                    truncated,
                    original_size: bytes,
                    note: None,
                });
                dest
            }
            InboundKind::Doc(doc) => {
                let extracted = tokio::select! {
                    _ = cancel.cancelled() => {
                        let _ = std::fs::remove_file(&dest);
                        return Err(InboundError::Cancelled);
                    }
                    r = extract_text_with(doc, &dest, opts.ocr.as_ref(), &opts.convert) => r,
                };
                // The original is not needed once converted (Discord does
                // the same); failure or success, it goes.
                let _ = std::fs::remove_file(&dest);
                let extracted = match extracted {
                    Ok(x) => x,
                    Err(e) => {
                        warn!(file_id = %f.id, "slack inbound conversion failed: {e:#}");
                        reject(
                            &mut msg,
                            &name,
                            RejectReason::Unavailable(conversion_reason(&e)),
                        );
                        continue;
                    }
                };
                let text_path = dest.with_extension(format!(
                    "{}.txt",
                    dest.extension().and_then(|e| e.to_str()).unwrap_or("doc")
                ));
                let (to_write, truncated) =
                    augmentagent_docs::inbound::truncate_text_bytes(extracted.text.as_bytes());
                if let Err(e) = write_private(&text_path, to_write) {
                    let why = format!("couldn't store the extracted text ({e})");
                    reject(&mut msg, &name, RejectReason::Unavailable(why));
                    continue;
                }
                msg.text_files.push(TextAttachment {
                    path: text_path.clone(),
                    truncated,
                    original_size: extracted.text.len() as u64,
                    note: extracted.ocr.note(),
                });
                text_path
            }
        };
        msg.accepted.push(AcceptedFile {
            file_id: f.id.clone(),
            original_name: name,
            kind,
            path: accepted_path,
            downloaded_bytes: bytes,
        });
    }

    msg.prompt = build_prompt(&msg.user_text, &msg.images, &msg.text_files);
    Ok(msg)
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    if let Err(e) = file.write_all(bytes) {
        let _ = std::fs::remove_file(path);
        return Err(e);
    }
    Ok(())
}
