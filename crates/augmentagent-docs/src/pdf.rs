//! Local PDF exports confined to the caller's wiki root (#992).
use anyhow::{bail, Context, Result};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

pub const MAX_PDF_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_MARKDOWN_BYTES: usize = 1024 * 1024;
const WORKER: &str = include_str!("../python/render_pdf.py");

#[derive(Debug, serde::Serialize)]
pub struct PdfReceipt {
    pub path: PathBuf,
    pub bytes: usize,
    pub attach: String,
}

async fn bounded_read(reader: impl AsyncRead + Unpin, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > limit {
        bail!("PDF renderer output exceeds {limit} bytes");
    }
    Ok(bytes)
}

/// Neither document content nor paths are interpreted as shell commands.
/// The worker is embedded so it also works with cwd set to the wiki directory.
pub async fn render_pdf(root: &Path, input: &Path, output: Option<&Path>) -> Result<PdfReceipt> {
    let root = root.canonicalize().context("resolve WIKI_ROOT")?;
    if !root.is_dir() {
        bail!("WIKI_ROOT must be a directory");
    }
    let source = root
        .join(input)
        .canonicalize()
        .context("resolve Markdown input")?;
    if !source.starts_with(&root) {
        bail!("Markdown input must be under WIKI_ROOT");
    }
    if !matches!(
        source.extension().and_then(|s| s.to_str()),
        Some("md" | "markdown" | "txt")
    ) {
        bail!("PDF input must be a .md, .markdown or .txt file");
    }
    let destination = output
        .map(|p| root.join(p))
        .unwrap_or_else(|| source.with_extension("pdf"));
    if destination.extension().and_then(|s| s.to_str()) != Some("pdf") {
        bail!("PDF output must have a .pdf extension");
    }
    let parent = destination
        .parent()
        .context("PDF output needs a parent directory")?
        .canonicalize()
        .context("PDF output directory must already exist")?;
    if !parent.starts_with(&root) {
        bail!("PDF output must be under WIKI_ROOT");
    }
    let destination = parent.join(
        destination
            .file_name()
            .context("PDF output needs a filename")?,
    );
    let relative = destination
        .strip_prefix(&root)?
        .to_str()
        .context("PDF output path must be UTF-8")?;
    if relative.trim() != relative || relative.chars().any(char::is_control) {
        bail!("PDF output path cannot contain control characters or surrounding whitespace");
    }
    if destination.symlink_metadata().is_ok() {
        bail!("PDF output already exists; choose a new filename");
    }
    if !source.metadata()?.is_file() {
        bail!("Markdown input must be a regular file");
    }
    let source_file = std::fs::File::open(&source).context("open Markdown input")?;
    if !source_file.metadata()?.is_file() {
        bail!("Markdown input must be a regular file");
    }
    let mut markdown = String::new();
    source_file
        .take(MAX_MARKDOWN_BYTES as u64 + 1)
        .read_to_string(&mut markdown)
        .context("read UTF-8 Markdown input")?;
    if markdown.len() > MAX_MARKDOWN_BYTES {
        bail!("Markdown input exceeds 1 MiB");
    }
    if markdown.trim().is_empty() {
        bail!("cannot render an empty document");
    }

    let mut child = tokio::process::Command::new("python3")
        .args(["-E", "-P", "-c", WORKER])
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .kill_on_drop(true).spawn()
        .context("start PDF renderer: install python3, python3-reportlab, python3-markdown and fonts-liberation")?;
    let mut stdin = child.stdin.take().context("PDF renderer stdin")?;
    let stdout = child.stdout.take().context("PDF renderer stdout")?;
    let stderr = child.stderr.take().context("PDF renderer stderr")?;
    let ((), pdf, errors, status) = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::try_join!(
            async {
                if let Err(error) = stdin.write_all(markdown.as_bytes()).await {
                    // An unavailable dependency can make Python exit before
                    // consuming stdin. Preserve its actionable stderr below.
                    if error.kind() != std::io::ErrorKind::BrokenPipe {
                        return Err(error.into());
                    }
                }
                drop(stdin);
                Ok::<_, anyhow::Error>(())
            },
            bounded_read(stdout, MAX_PDF_BYTES),
            bounded_read(stderr, 64 * 1024),
            async { Ok::<_, anyhow::Error>(child.wait().await?) },
        )
    })
    .await
    .context("PDF rendering timed out after 30 seconds")??;
    if !status.success() {
        bail!("PDF renderer failed: {}. Required packages: python3-reportlab, python3-markdown, fonts-liberation", String::from_utf8_lossy(&errors).trim());
    }
    if !pdf.starts_with(b"%PDF-") || !pdf.ends_with(b"%%EOF\n") {
        bail!("PDF renderer did not return a complete PDF");
    }
    // Publish only a complete, size-checked PDF. Never clobber an existing
    // file (including a symlink), even if it appeared while rendering.
    let mut temporary = tempfile::NamedTempFile::new_in(&parent)?;
    temporary.write_all(&pdf)?;
    temporary
        .persist_noclobber(&destination)
        .context("publish PDF without overwriting an existing file")?;
    Ok(PdfReceipt {
        bytes: pdf.len(),
        attach: format!("ATTACH: {relative}"),
        path: destination,
    })
}
