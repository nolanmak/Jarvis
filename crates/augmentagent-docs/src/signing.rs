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
const RUNTIME_PINS: &str = include_str!("../python/requirements.txt");
/// Identifies the code that turns a placement into bytes: the worker and the
/// pinned runtime. A proposal's output hash is only reproducible under it.
pub fn worker_fingerprint() -> String {
    digest(format!("{WORKER}\0{RUNTIME_PINS}").as_bytes())
}
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
    register_marker(state, "generated", b"generated-pdf-v1\n", pdf)
}
/// Register the exact bytes of a PDF the allowlisted owner attached in
/// Discord. Same trust as a Jarvis render: the owner chose this document, and
/// the marker lives outside the agent-writable wiki so the agent can't forge it.
pub fn register_owner_upload(pdf: &[u8]) -> Result<()> {
    register_owner_upload_at(&state_root()?, pdf)
}
pub fn register_owner_upload_at(state: &Path, pdf: &[u8]) -> Result<()> {
    register_marker(state, "uploads", b"owner-upload-v1\n", pdf)
}
/// Keep an owner-attached PDF byte-for-byte inside WIKI_ROOT and mark it
/// signable. Returns the wiki-relative path `doc sign` takes. The staged bytes
/// are re-read and hash-checked, so what is signed is exactly what was sent.
pub fn stage_owner_upload(root: &Path, state: &Path, filename: &str, pdf: &[u8]) -> Result<PathBuf> {
    if !pdf.starts_with(b"%PDF-") {
        bail!("attachment is not a PDF");
    }
    let stem = Path::new(filename)
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("document");
    let path = crate::delivery::stage(root, &format!("{stem}.pdf"), pdf)?;
    if digest(&read(&path, crate::pdf::MAX_PDF_BYTES)?) != digest(pdf) {
        bail!("staged upload does not match the attached bytes");
    }
    register_owner_upload_at(state, pdf)?;
    Ok(path.strip_prefix(root.canonicalize()?)?.to_path_buf())
}
fn is_trusted(state: &Path, sha256: &str) -> bool {
    ["generated", "uploads"]
        .iter()
        .any(|dir| state.join(dir).join(sha256).is_file())
}
fn register_marker(state: &Path, kind: &str, tag: &[u8], pdf: &[u8]) -> Result<()> {
    let dir = state.join(kind);
    private_dir(&dir)?;
    let path = dir.join(digest(pdf));
    // Multiple renders can produce the same bytes. Publish atomically, accepting
    // only another completed regular marker as the idempotent winner.
    let mut marker = tempfile::NamedTempFile::new_in(&dir)?;
    marker.write_all(tag)?;
    marker.as_file().sync_all()?;
    match marker.persist_noclobber(&path) {
        Ok(_) => Ok(()),
        Err(error)
            if error.error.kind() == std::io::ErrorKind::AlreadyExists
                && path.symlink_metadata().is_ok_and(|m| m.is_file()) =>
        {
            Ok(())
        }
        Err(error) => Err(error).context("register generated PDF"),
    }
}
/// The exact image box the worker draws, PDF points from the bottom-left.
/// Proposals made since #1510 also carry `ink` (the visible signature, which
/// is what was aligned and overlap-checked) and, when placed on a detected
/// line, its `anchor`. Older proposals have neither and keep their meaning.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Placement {
    pub page: u32,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ink: Option<InkBox>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<Anchor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date_x: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date_y: Option<f64>,
}
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct InkBox {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}
/// The detected signature line a placement was aligned to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Anchor {
    pub line: u32,
    pub x0: f64,
    pub x1: f64,
    pub top: f64,
    pub text: Option<String>,
}
/// Where the owner wants the signature. Anchors are resolved against the
/// rendered page; explicit coordinates address the visible ink's bottom-left.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    /// Text directly under the signature line, e.g. the signer's name.
    AnchorText(String),
    /// 1-based index from `sign_lines`, top to bottom.
    Line(u32),
    Ink { x: f64, y: f64, width: f64 },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlacementRequest {
    pub page: u32,
    pub target: Target,
    /// Ink width for anchored placement; defaults to min(180, line length − inset).
    pub width: Option<f64>,
    pub date: Option<String>,
}
impl PlacementRequest {
    pub fn ink(page: u32, x: f64, y: f64, width: f64, date: Option<String>) -> Self {
        Self { page, target: Target::Ink { x, y, width }, width: None, date }
    }
    pub fn anchor_text(page: u32, text: &str) -> Self {
        Self { page, target: Target::AnchorText(text.into()), width: None, date: None }
    }
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
    /// SHA-256 of the signed bytes the preview was rendered from (#1510).
    /// Signing is deterministic, so approval publishes exactly these bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_output_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_sha256: Option<String>,
}
/// A proposal plus the PNG of its signed page, for the approval card.
pub struct Proposed {
    pub proposal: Proposal,
    pub preview_png: Vec<u8>,
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
const PREVIEW_DPI: u32 = 150;
const LAYOUT_DPI: u32 = 300;
async fn run(mut command: tokio::process::Command, what: &str) -> Result<Vec<u8>> {
    let out = tokio::time::timeout(Duration::from_secs(30), command.kill_on_drop(true).output())
        .await
        .with_context(|| format!("{what} timed out"))?
        .with_context(|| format!("start {what}; install poppler-utils"))?;
    if !out.status.success() {
        bail!("{what} failed: {}", String::from_utf8_lossy(&out.stderr).chars().take(300).collect::<String>().trim());
    }
    Ok(out.stdout)
}
/// Render one page with Poppler to PNG bytes.
async fn render_png(pdf: &[u8], page: u32, dpi: u32, gray: bool) -> Result<Vec<u8>> {
    let dir = tempfile::tempdir()?;
    let input = dir.path().join("in.pdf");
    std::fs::write(&input, pdf)?;
    let base = dir.path().join("page");
    let mut cmd = tokio::process::Command::new("pdftoppm");
    cmd.args(["-png", "-singlefile", "-r", &dpi.to_string(), "-f", &page.to_string(), "-l", &page.to_string()]);
    if gray {
        cmd.arg("-gray");
    }
    cmd.arg(&input).arg(&base);
    run(cmd, "pdftoppm").await?;
    read(&base.with_extension("png"), 32 * 1024 * 1024)
}
fn unescape(text: &str) -> String {
    text.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'")
        .replace("&#39;", "'").replace("&amp;", "&")
}
/// Words on one page from `pdftotext -bbox`: [x0, top, x1, bottom, text], top-left origin.
async fn page_words(pdf: &[u8], page: u32) -> Result<Vec<serde_json::Value>> {
    let dir = tempfile::tempdir()?;
    let input = dir.path().join("in.pdf");
    std::fs::write(&input, pdf)?;
    let mut cmd = tokio::process::Command::new("pdftotext");
    cmd.args(["-bbox", "-f", &page.to_string(), "-l", &page.to_string()]).arg(&input).arg("-");
    let html = String::from_utf8(run(cmd, "pdftotext").await?)?;
    let attr = |line: &str, name: &str| -> Option<f64> {
        let at = line.find(&format!("{name}=\""))? + name.len() + 2;
        line[at..].split('"').next()?.parse().ok()
    };
    let mut words = Vec::new();
    for line in html.lines().map(str::trim).filter(|l| l.starts_with("<word ")) {
        let text = line.split_once('>').and_then(|(_, rest)| rest.strip_suffix("</word>"));
        if let (Some(x0), Some(top), Some(x1), Some(bottom), Some(text)) =
            (attr(line, "xMin"), attr(line, "yMin"), attr(line, "xMax"), attr(line, "yMax"), text)
        {
            words.push(serde_json::json!([x0, top, x1, bottom, unescape(text)]));
        }
    }
    Ok(words)
}
/// Page count, and refuse to rasterize a page too large to measure (a huge
/// MediaBox would cost gigabytes at 300 dpi before anything validated it).
async fn page_count_checked(pdf: &[u8], page: Option<u32>) -> Result<u32> {
    let info: serde_json::Value = serde_json::from_slice(
        &worker(serde_json::json!({"op": "info", "pdf": STANDARD.encode(pdf)})).await?,
    )?;
    let pages = info["pages"].as_u64().context("page count")? as u32;
    let max = info["max_pt"].as_f64().context("size limit")?;
    let sizes = info["sizes"].as_array().context("page sizes")?;
    for (i, size) in sizes.iter().enumerate() {
        let n = i as u32 + 1;
        if page.is_some_and(|p| p != n) {
            continue;
        }
        let (w, h) = (size[0].as_f64().unwrap_or(f64::MAX), size[1].as_f64().unwrap_or(f64::MAX));
        if !(w > 0. && h > 0. && w <= max && h <= max) {
            bail!("page {n} is {w}x{h} pt; pages larger than {max} pt cannot be measured for signing");
        }
    }
    Ok(pages)
}
async fn layout(pdf: &[u8], page: u32) -> Result<serde_json::Value> {
    let raster = render_png(pdf, page, LAYOUT_DPI, true).await?;
    Ok(serde_json::json!({
        "dpi": LAYOUT_DPI,
        "raster": STANDARD.encode(raster),
        "words": page_words(pdf, page).await?,
    }))
}
async fn worker(request: serde_json::Value) -> Result<Vec<u8>> {
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
    let (_, out, errors, status) = tokio::time::timeout(Duration::from_secs(60), async {
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
async fn sign_bytes(pdf: &[u8], png: &[u8], placement: &Placement) -> Result<Vec<u8>> {
    let mut request = serde_json::to_value(placement)?;
    request["op"] = "sign".into();
    request["pdf"] = STANDARD.encode(pdf).into();
    request["png"] = STANDARD.encode(png).into();
    worker(request).await
}
/// Horizontal rules (signature lines) a reader sees on each page, with the
/// text directly beneath them. Read-only; any PDF inside WIKI_ROOT.
pub async fn sign_lines(root: &Path, source: &Path, page: Option<u32>) -> Result<Vec<serde_json::Value>> {
    let source = scoped(&root.canonicalize()?, source, true)?;
    let pdf = read(&source, crate::pdf::MAX_PDF_BYTES)?;
    let pages = page_count_checked(&pdf, page).await?;
    let mut all = Vec::new();
    let (mut current, last) = match page {
        Some(p) if p == 0 || p > pages => bail!("page number is outside the document"),
        Some(p) => (p, p),
        None => (1, pages),
    };
    while current <= last {
        let mut request = serde_json::json!({"op": "lines", "page": current, "layout": layout(&pdf, current).await?});
        request["pdf"] = STANDARD.encode(&pdf).into();
        let result: serde_json::Value = serde_json::from_slice(&worker(request).await?)?;
        for mut line in result["lines"].as_array().cloned().unwrap_or_default() {
            line["page"] = current.into();
            all.push(line);
        }
        current += 1;
    }
    Ok(all)
}
pub async fn propose(
    root: &Path,
    state: &Path,
    source: &Path,
    output: &Path,
    signature: &str,
    request: PlacementRequest,
) -> Result<Proposal> {
    Ok(propose_with_preview(root, state, source, output, signature, request).await?.proposal)
}
/// Resolve the placement against the rendered page, sign in memory, and
/// render the signed page: the owner approves that picture, and approval
/// publishes exactly the bytes it was rendered from.
pub async fn propose_with_preview(
    root: &Path,
    state: &Path,
    source: &Path,
    output: &Path,
    signature: &str,
    request: PlacementRequest,
) -> Result<Proposed> {
    let root = root.canonicalize()?;
    let source = scoped(&root, source, true)?;
    let output = scoped(&root, output, false)?;
    if output.symlink_metadata().is_ok() {
        bail!("output already exists; choose a new filename");
    }
    let pdf = read(&source, crate::pdf::MAX_PDF_BYTES)?;
    let document_sha256 = digest(&pdf);
    if !is_trusted(state, &document_sha256) {
        bail!("PDF is neither rendered by Jarvis nor uploaded by the owner; render it with doc render-pdf or have the owner attach it in Discord");
    }
    let png =
        read(&asset_path(state, signature)?, 1024 * 1024).context("signature is not installed")?;
    if request.page == 0 || request.page > page_count_checked(&pdf, Some(request.page)).await? {
        bail!("page number is outside the document");
    }
    let mut call = serde_json::to_value(&request)?;
    call["op"] = "resolve".into();
    call["pdf"] = STANDARD.encode(&pdf).into();
    call["png"] = STANDARD.encode(&png).into();
    call["layout"] = layout(&pdf, request.page).await?;
    let mut resolved: serde_json::Value = serde_json::from_slice(&worker(call).await?)?;
    let pages = resolved["pages"].as_u64().context("page count")? as usize;
    resolved["page"] = request.page.into();
    resolved["date"] = serde_json::to_value(&request.date)?;
    let placement: Placement = serde_json::from_value(resolved).context("resolved placement")?;
    let signed = sign_bytes(&pdf, &png, &placement).await?;
    let preview_png = render_png(&signed, placement.page, PREVIEW_DPI, false).await?;
    Ok(Proposed {
        proposal: Proposal {
            source: source.strip_prefix(&root)?.to_str().context("path")?.into(),
            output: output.strip_prefix(&root)?.to_str().context("path")?.into(),
            document_sha256,
            signature: signature.into(),
            signature_sha256: digest(&png),
            placement,
            pages,
            expected_output_sha256: Some(digest(&signed)),
            preview_sha256: Some(digest(&preview_png)),
            worker_sha256: Some(worker_fingerprint()),
        },
        preview_png,
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
    if !is_trusted(state, &proposal.document_sha256) {
        bail!("PDF provenance registration is missing");
    }
    let signed = sign_bytes(&pdf, &png, &proposal.placement).await?;
    if let Some(expected) = &proposal.expected_output_sha256 {
        if proposal.worker_sha256.as_deref() != Some(worker_fingerprint().as_str()) {
            bail!("Jarvis's signer was updated after this card was proposed, so the preview may not match; nothing was published. Propose the signature again");
        }
        if &digest(&signed) != expected {
            bail!("signed output differs from the approved preview; nothing was published");
        }
    }
    // One-for-one: the signed file must be the original bytes plus an appended
    // incremental update. Anything else is a re-encoding and is never published.
    if signed.len() <= pdf.len() || !signed.starts_with(&pdf) {
        bail!("worker output does not preserve the original document bytes; nothing was published");
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
