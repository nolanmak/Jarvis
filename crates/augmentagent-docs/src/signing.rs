//! Owner signature assets and revision-bound PDF signing. No unattended CLI entrypoint.
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::io::AsyncWriteExt;

const WORKER: &str = include_str!("../python/sign_pdf.py");
pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub fn state_root() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/state")))
        .context("HOME or XDG_STATE_HOME is required")?;
    Ok(base.join("augmentagent/signatures"))
}
fn private_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
fn read(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let meta = path.symlink_metadata()?;
    if !meta.is_file() || meta.len() > limit as u64 {
        bail!("expected a bounded regular file");
    }
    let f = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    f.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        bail!("file exceeds size limit");
    }
    Ok(bytes)
}
fn publish(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = tempfile::NamedTempFile::new_in(path.parent().context("missing parent")?)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist_noclobber(path)
        .context("destination already exists or cannot be published")?;
    Ok(())
}
fn asset_path(state: &Path, name: &str) -> Result<PathBuf> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        bail!("signature name must contain only ASCII letters, digits, dash or underscore");
    }
    Ok(state.join(format!("{name}.png")))
}
/// Operator-only ingestion. Existing names are immutable; install a new named variant to rotate.
pub fn install(state: &Path, name: &str, source: &Path) -> Result<String> {
    let bytes = read(source, 1024 * 1024)?;
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        bail!("signature must be a PNG");
    }
    private_dir(state)?;
    let path = asset_path(state, name)?;
    if path.exists() && read(&path, 1024 * 1024)? == bytes {
        return Ok(digest(&bytes));
    }
    publish(&path, &bytes)?;
    Ok(digest(&bytes))
}
pub fn list(state: &Path) -> Result<Vec<String>> {
    if !state.exists() {
        return Ok(vec![]);
    }
    let mut names = Vec::new();
    for entry in std::fs::read_dir(state)? {
        let p = entry?.path();
        if p.extension().and_then(|s| s.to_str()) == Some("png") && p.symlink_metadata()?.is_file()
        {
            if let Some(n) = p.file_stem().and_then(|s| s.to_str()) {
                names.push(n.to_string());
            }
        }
    }
    names.sort();
    Ok(names)
}
/// Register the exact bytes produced by our renderer, outside the agent-writable wiki.
pub fn register_generated(pdf: &[u8]) -> Result<()> {
    register_at(&state_root()?, pdf)
}
pub fn register_at(state: &Path, pdf: &[u8]) -> Result<()> {
    let dir = state.join("generated");
    private_dir(&dir)?;
    let path = dir.join(digest(pdf));
    if !path.exists() {
        publish(&path, b"generated-pdf-v1\n")?;
    }
    Ok(())
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Placement {
    pub page: u32,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub date: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Proposal {
    pub source: String,
    pub output: String,
    pub document_sha256: String,
    pub signature: String,
    pub signature_sha256: String,
    pub placement: Placement,
    pub pages: usize,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Receipt {
    pub proposal: Proposal,
    pub output_sha256: String,
    pub attach: String,
}
fn scoped(root: &Path, path: &Path, exists: bool) -> Result<PathBuf> {
    let root = root.canonicalize()?;
    let path = root.join(path);
    let p = if exists {
        path.canonicalize()?
    } else {
        path.parent()
            .context("missing parent")?
            .canonicalize()?
            .join(path.file_name().context("missing filename")?)
    };
    if !p.starts_with(&root) || p.extension().and_then(|s| s.to_str()) != Some("pdf") {
        bail!("PDF must be inside WIKI_ROOT and have a .pdf extension");
    }
    let relative = p
        .strip_prefix(&root)?
        .to_str()
        .context("UTF-8 path required")?;
    if relative.trim() != relative || relative.chars().any(char::is_control) {
        bail!("invalid PDF path");
    }
    Ok(p)
}
async fn worker(pdf: &[u8], png: &[u8], placement: &Placement, sign: bool) -> Result<Vec<u8>> {
    let mut request = serde_json::to_value(placement)?;
    request["pdf"] = STANDARD.encode(pdf).into();
    request["png"] = STANDARD.encode(png).into();
    request["sign"] = sign.into();
    let bytes = serde_json::to_vec(&request)?;
    let mut child = tokio::process::Command::new(crate::pdf::renderer_python()?)
        .args(["-I", "-c", WORKER])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("start signing runtime; run scripts/pdf-runtime.py")?;
    let mut stdin = child.stdin.take().context("worker stdin")?;
    let stdout = child.stdout.take().context("worker stdout")?;
    let stderr = child.stderr.take().context("worker stderr")?;
    let (_, out, errors, status) = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::try_join!(
            async {
                stdin.write_all(&bytes).await?;
                drop(stdin);
                Ok::<_, anyhow::Error>(())
            },
            crate::pdf::bounded_read(stdout, crate::pdf::MAX_PDF_BYTES),
            crate::pdf::bounded_read(stderr, 2048),
            async { Ok::<_, anyhow::Error>(child.wait().await?) }
        )
    })
    .await
    .context("signing worker timed out")??;
    if !status.success() {
        bail!("{}", String::from_utf8_lossy(&errors).trim());
    }
    Ok(out)
}
pub async fn propose(
    root: &Path,
    state: &Path,
    source: &Path,
    output: &Path,
    signature: &str,
    placement: Placement,
) -> Result<Proposal> {
    let root = root.canonicalize()?;
    let source = scoped(&root, source, true)?;
    let output = scoped(&root, output, false)?;
    if output.symlink_metadata().is_ok() {
        bail!("output already exists; choose a new filename");
    }
    let pdf = read(&source, crate::pdf::MAX_PDF_BYTES)?;
    let document_sha256 = digest(&pdf);
    if !state.join("generated").join(&document_sha256).is_file() {
        bail!("PDF is not registered as generated by Jarvis; render the reviewed source with doc render-pdf first");
    }
    let png =
        read(&asset_path(state, signature)?, 1024 * 1024).context("signature is not installed")?;
    let info: serde_json::Value =
        serde_json::from_slice(&worker(&pdf, &png, &placement, false).await?)?;
    Ok(Proposal {
        source: source.strip_prefix(&root)?.to_str().context("path")?.into(),
        output: output.strip_prefix(&root)?.to_str().context("path")?.into(),
        document_sha256,
        signature: signature.into(),
        signature_sha256: digest(&png),
        placement,
        pages: info["pages"].as_u64().context("page count")? as usize,
    })
}
/// Called only by the approved action handler, behind its durable execution fence.
pub async fn execute(root: &Path, state: &Path, proposal: &Proposal) -> Result<Receipt> {
    let source = scoped(root, Path::new(&proposal.source), true)?;
    let output = scoped(root, Path::new(&proposal.output), false)?;
    if output.symlink_metadata().is_ok() {
        bail!("output already exists; nothing was overwritten");
    }
    let pdf = read(&source, crate::pdf::MAX_PDF_BYTES)?;
    let png = read(&asset_path(state, &proposal.signature)?, 1024 * 1024)?;
    if digest(&pdf) != proposal.document_sha256 || digest(&png) != proposal.signature_sha256 {
        bail!("document or signature changed after proposal; request a fresh approval");
    }
    if !state
        .join("generated")
        .join(&proposal.document_sha256)
        .is_file()
    {
        bail!("generated PDF registration is missing");
    }
    let signed = worker(&pdf, &png, &proposal.placement, true).await?;
    if !signed.starts_with(b"%PDF-") {
        bail!("worker returned invalid PDF");
    }
    // Re-resolve after the worker: fail if a directory was moved/replaced meanwhile.
    if scoped(root, Path::new(&proposal.output), false)? != output {
        bail!("output directory changed");
    }
    publish(&output, &signed)?;
    Ok(Receipt {
        proposal: proposal.clone(),
        output_sha256: digest(&signed),
        attach: format!("ATTACH: {}", proposal.output),
    })
}
pub fn verify_receipt(root: &Path, receipt: &Receipt) -> Result<()> {
    let p = scoped(root, Path::new(&receipt.proposal.output), true)?;
    if digest(&read(&p, crate::pdf::MAX_PDF_BYTES)?) != receipt.output_sha256 {
        bail!("signed output changed; inspect it before proceeding");
    }
    Ok(())
}

/// The attachment must be the same bounded bytes whose hash is on the card.
pub fn review_bytes(root: &Path, proposal: &Proposal) -> Result<Vec<u8>> {
    let source = scoped(root, Path::new(&proposal.source), true)?;
    let bytes = read(&source, crate::pdf::MAX_PDF_BYTES)?;
    if digest(&bytes) != proposal.document_sha256 {
        bail!("document changed before card upload; propose again");
    }
    Ok(bytes)
}

/// Read only bytes matching the immutable receipt before delivering an attachment.
pub fn verified_output(path: &Path, expected_sha256: &str) -> Result<Vec<u8>> {
    let bytes = read(path, crate::pdf::MAX_PDF_BYTES)?;
    if digest(&bytes) != expected_sha256 {
        bail!("signed output changed");
    }
    Ok(bytes)
}
