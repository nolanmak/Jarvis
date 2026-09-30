//! Shared document → text pipeline (#939).
//!
//! Two stages, chosen by the owner on 2026-09-02:
//!
//! 1. **Text layer** — shell out to the local converter (`pdftotext` for PDF,
//!    `pandoc` for DOCX/DOC). Free, no network, handles every digital PDF.
//! 2. **OCR** — only when stage 1 comes back (near-)empty on a PDF, i.e. the
//!    file is a scan / photo with no text layer (the landlord's pest-control
//!    report that motivated the issue). Sends the PDF to Mistral OCR
//!    ([`OcrClient`]); no rasterization step, no Tesseract path.
//!
//! Stage 2 is gated on `MISTRAL_API_KEY` (keyring first, env fallback). When
//! the key is unset the OCR stage is *skipped* — no HTTP call, no error — and
//! the caller gets stage 1's empty text plus [`OcrOutcome::Unavailable`], whose
//! [`OcrOutcome::note`] spells out why so the agent can ask for screenshots
//! instead of staring at a blank. Provisioning the key later is a pure config
//! change.
//!
//! This is its own leaf crate because both the Discord attachment handler
//! (`augmentagent-approval-discord`) and the CLI need it, and channel-core
//! depends on approval-discord — the pipeline can't live in core without a
//! dependency cycle.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tracing::{debug, info, warn};

mod ocr;
pub mod pdf;

pub use ocr::{OcrClient, OcrResult, DEFAULT_OCR_BASE_URL, DEFAULT_OCR_MODEL, MAX_OCR_BYTES};

/// Env var / keyring slot carrying the Mistral API key.
pub const MISTRAL_API_KEY_ENV: &str = "MISTRAL_API_KEY";
/// Optional override for the OCR model (default [`DEFAULT_OCR_MODEL`]).
pub const OCR_MODEL_ENV: &str = "AUGMENTAGENT_MISTRAL_OCR_MODEL";
/// Optional override for the Mistral API base URL (tests / proxies).
pub const OCR_BASE_URL_ENV: &str = "AUGMENTAGENT_MISTRAL_BASE_URL";

/// Stage-1 output with fewer than this many non-whitespace characters counts
/// as "no text layer". `pdftotext` emits one form feed per page on image-only
/// PDFs, and a scanned page occasionally carries a stray header fragment; a
/// real page of prose is hundreds of characters.
pub const NEAR_EMPTY_THRESHOLD: usize = 20;

/// Document formats the pipeline converts. Everything else flows through the
/// callers' regular text / image branches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocKind {
    Pdf,
    Docx,
    Doc,
}

impl DocKind {
    /// Canonical MIME type for the kind.
    pub fn mime(self) -> &'static str {
        match self {
            DocKind::Pdf => "application/pdf",
            DocKind::Docx => {
                "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
            }
            DocKind::Doc => "application/msword",
        }
    }

    /// Whether stage 2 (OCR) can apply. DOCX/DOC carry text by construction.
    fn ocr_eligible(self) -> bool {
        matches!(self, DocKind::Pdf)
    }
}

/// Identify the doc kind from a filename and/or MIME type. Returns `None` for
/// everything else. MIME wins when present; the (case-insensitive) extension
/// is the fallback because Discord sometimes omits `content_type`.
pub fn doc_kind_for(filename: &str, content_type: Option<&str>) -> Option<DocKind> {
    let ext = Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase());
    match (content_type, ext.as_deref()) {
        (Some("application/pdf"), _) | (_, Some("pdf")) => Some(DocKind::Pdf),
        (Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document"), _)
        | (_, Some("docx")) => Some(DocKind::Docx),
        (Some("application/msword"), _) | (_, Some("doc")) => Some(DocKind::Doc),
        _ => None,
    }
}

/// Pure helper that picks the stage-1 converter binary + args for a doc kind.
/// Extracted so the dispatch is unit-testable without the binaries installed.
pub fn doc_command_for(kind: DocKind, in_path: &Path) -> (&'static str, Vec<String>) {
    let in_arg = in_path.to_string_lossy().into_owned();
    match kind {
        // `-` writes to stdout; `-layout` preserves columns/whitespace better
        // for log-like dumps and tables.
        DocKind::Pdf => ("pdftotext", vec!["-layout".into(), in_arg, "-".into()]),
        // pandoc handles both .docx and legacy .doc.
        DocKind::Docx | DocKind::Doc => ("pandoc", vec!["--to=plain".into(), in_arg]),
    }
}

/// Upper bound for one converter run. A stuck pdftotext/pandoc is killed
/// and the turn gets an error instead of hanging.
pub const CONVERT_TIMEOUT: Duration = Duration::from_secs(120);

/// Directories searched after `PATH`. A launchd job gets
/// `/usr/bin:/bin:/usr/sbin:/sbin` unless its plist sets `PATH`
/// (`scripts/lib/launchd-install.sh` does, systemd units do too); these
/// cover Homebrew on Apple Silicon and Intel and the distro locations when a
/// service was installed without them.
pub const SERVICE_TOOL_DIRS: &[&str] = &["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"];

/// How stage 1 finds and bounds its converter.
#[derive(Debug, Clone)]
pub struct ConvertOptions {
    /// `PATH` to search; `None` = only `fallback_dirs`.
    pub search_path: Option<OsString>,
    /// Searched after `search_path`, in order.
    pub fallback_dirs: Vec<PathBuf>,
    pub timeout: Duration,
}

impl Default for ConvertOptions {
    /// The process `PATH`, then [`SERVICE_TOOL_DIRS`], bounded by
    /// [`CONVERT_TIMEOUT`].
    fn default() -> Self {
        Self {
            search_path: std::env::var_os("PATH"),
            fallback_dirs: SERVICE_TOOL_DIRS.iter().map(PathBuf::from).collect(),
            timeout: CONVERT_TIMEOUT,
        }
    }
}

/// First executable regular file named `program` on the search path, then in
/// the fallback directories.
pub fn resolve_tool(program: &str, opts: &ConvertOptions) -> Option<PathBuf> {
    let on_path = opts
        .search_path
        .as_deref()
        .map(|p| std::env::split_paths(p).collect::<Vec<_>>())
        .unwrap_or_default();
    on_path
        .into_iter()
        .chain(opts.fallback_dirs.iter().cloned())
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable_file(candidate))
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
    }
}

fn install_hint(program: &str) -> &'static str {
    match program {
        "pdftotext" => "install poppler (`brew install poppler` on macOS, `apt install poppler-utils` on Linux)",
        "pandoc" => "install pandoc (`brew install pandoc` on macOS, `apt install pandoc` on Linux)",
        _ => "install it",
    }
}

/// Stage 1: shell out to the converter and return the extracted text. Errors
/// include: binary missing, non-zero exit, unreadable input.
pub async fn convert_doc_to_text(kind: DocKind, in_path: &Path) -> anyhow::Result<String> {
    convert_doc_to_text_with(kind, in_path, &ConvertOptions::default()).await
}

/// [`convert_doc_to_text`] with explicit tool lookup and timeout.
pub async fn convert_doc_to_text_with(
    kind: DocKind,
    in_path: &Path,
    opts: &ConvertOptions,
) -> anyhow::Result<String> {
    let (program, args) = doc_command_for(kind, in_path);
    let Some(binary) = resolve_tool(program, opts) else {
        let dirs: Vec<String> = opts
            .fallback_dirs
            .iter()
            .map(|d| d.display().to_string())
            .collect();
        anyhow::bail!(
            "{program} is not installed or not on the service PATH (also looked in {}); {}",
            if dirs.is_empty() {
                "no other directories".to_string()
            } else {
                dirs.join(", ")
            },
            install_hint(program)
        );
    };
    // kill_on_drop: when the timeout (or a cancelled turn) drops the future,
    // the converter is killed instead of running on in the background.
    let run = tokio::process::Command::new(&binary)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = match tokio::time::timeout(opts.timeout, run).await {
        Err(_elapsed) => anyhow::bail!(
            "{program} timed out after {}s and was stopped",
            opts.timeout.as_secs_f32()
        ),
        Ok(result) => result.map_err(|e| anyhow::anyhow!("spawn {program}: {e}"))?,
    };
    if !output.status.success() {
        anyhow::bail!(
            "{program} exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// `true` when `text` has fewer than [`NEAR_EMPTY_THRESHOLD`] non-whitespace
/// characters — the signal that a PDF has no usable text layer.
pub fn text_is_near_empty(text: &str) -> bool {
    text.chars().filter(|c| !c.is_whitespace()).count() < NEAR_EMPTY_THRESHOLD
}

/// What stage 2 did for one document. Carried alongside the text so callers
/// can annotate prompts / CLI output honestly instead of passing a silent
/// blank downstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OcrOutcome {
    /// Stage 1 produced usable text (or the kind never OCRs); OCR not consulted.
    NotNeeded,
    /// Stage 1 was empty and Mistral OCR recovered the text.
    Applied { pages: u32 },
    /// Stage 1 was empty and no `MISTRAL_API_KEY` is configured — OCR skipped.
    Unavailable,
    /// Stage 1 was empty and the OCR call failed (reason inside).
    Failed(String),
}

impl OcrOutcome {
    /// Human-readable annotation for prompts and CLI output. `None` when there
    /// is nothing worth saying (text layer was fine).
    pub fn note(&self) -> Option<String> {
        match self {
            OcrOutcome::NotNeeded => None,
            OcrOutcome::Applied { pages } => Some(format!(
                "text recovered via OCR ({} page{}) — this PDF had no text layer",
                pages,
                if *pages == 1 { "" } else { "s" }
            )),
            OcrOutcome::Unavailable => Some(
                "OCR unavailable: this PDF has no text layer (a scanned image) and \
                 MISTRAL_API_KEY is not configured, so the extraction is empty — \
                 ask the user to screenshot the pages, or configure MISTRAL_API_KEY"
                    .to_string(),
            ),
            OcrOutcome::Failed(reason) => Some(format!(
                "OCR failed: this PDF has no text layer (a scanned image) and the \
                 Mistral OCR call failed ({reason}), so the extraction is empty"
            )),
        }
    }

    /// Short status token for one-line CLI / log output.
    pub fn summary(&self) -> String {
        match self {
            OcrOutcome::NotNeeded => "not-needed".to_string(),
            OcrOutcome::Applied { pages } => {
                format!(
                    "applied ({} page{})",
                    pages,
                    if *pages == 1 { "" } else { "s" }
                )
            }
            OcrOutcome::Unavailable => {
                format!("unavailable ({MISTRAL_API_KEY_ENV} not set)")
            }
            OcrOutcome::Failed(reason) => format!("failed: {reason}"),
        }
    }
}

/// Result of [`extract_text`].
#[derive(Debug, Clone)]
pub struct Extracted {
    pub text: String,
    pub ocr: OcrOutcome,
}

/// Run the two-stage pipeline on one document.
///
/// - Stage 1 always runs. For DOCX/DOC that is the whole story.
/// - For a PDF whose stage-1 text is near-empty, stage 2 runs when `ocr` is
///   `Some`; with `None` the outcome is [`OcrOutcome::Unavailable`] and the
///   empty text is returned as-is (never an error — owner rule, #939).
/// - A stage-1 *failure* (converter missing / crashed) on a PDF still tries
///   OCR when available, since Mistral doesn't need `pdftotext`; without OCR
///   the stage-1 error propagates, matching pre-#939 behaviour.
/// - An OCR *failure* degrades to [`OcrOutcome::Failed`] with the empty
///   stage-1 text rather than dropping the attachment on the floor.
pub async fn extract_text(
    kind: DocKind,
    in_path: &Path,
    ocr: Option<&OcrClient>,
) -> anyhow::Result<Extracted> {
    extract_text_with(kind, in_path, ocr, &ConvertOptions::default()).await
}

/// [`extract_text`] with explicit converter lookup and timeout (#1293).
pub async fn extract_text_with(
    kind: DocKind,
    in_path: &Path,
    ocr: Option<&OcrClient>,
    opts: &ConvertOptions,
) -> anyhow::Result<Extracted> {
    // A missing input is a caller bug regardless of kind; surface it before
    // the converter produces a confusing "no such file" of its own.
    if !tokio::fs::try_exists(in_path).await.unwrap_or(false) {
        anyhow::bail!("document not found: {}", in_path.display());
    }

    let stage1 = convert_doc_to_text_with(kind, in_path, opts).await;

    if !kind.ocr_eligible() {
        return Ok(Extracted {
            text: stage1?,
            ocr: OcrOutcome::NotNeeded,
        });
    }

    let (text, stage1_err) = match stage1 {
        Ok(t) => (t, None),
        Err(e) => (String::new(), Some(e)),
    };
    if stage1_err.is_none() && !text_is_near_empty(&text) {
        return Ok(Extracted {
            text,
            ocr: OcrOutcome::NotNeeded,
        });
    }

    let Some(client) = ocr else {
        if let Some(e) = stage1_err {
            return Err(e.context("text-layer extraction failed and OCR is not configured"));
        }
        info!(
            path = %in_path.display(),
            "pdf has no text layer and {MISTRAL_API_KEY_ENV} is not set; OCR skipped"
        );
        return Ok(Extracted {
            text,
            ocr: OcrOutcome::Unavailable,
        });
    };

    debug!(path = %in_path.display(), model = client.model(), "pdf has no text layer; running OCR");
    let bytes = tokio::fs::read(in_path)
        .await
        .map_err(|e| anyhow::anyhow!("read {} for OCR: {e}", in_path.display()))?;
    match client.ocr_pdf(&bytes).await {
        Ok(OcrResult { markdown, pages }) => {
            info!(path = %in_path.display(), pages, model = client.model(), "OCR recovered text");
            Ok(Extracted {
                text: markdown,
                ocr: OcrOutcome::Applied { pages },
            })
        }
        Err(e) => {
            let reason = format!("{e:#}");
            warn!(path = %in_path.display(), "OCR failed: {reason}");
            if let Some(e1) = stage1_err {
                return Err(e1.context(format!("OCR fallback also failed: {reason}")));
            }
            Ok(Extracted {
                text,
                ocr: OcrOutcome::Failed(reason),
            })
        }
    }
}

/// Pure key-resolution rule (keyring first, env fallback, blanks are unset).
/// `OcrClient::from_env` feeds it the two live sources.
pub fn resolve_api_key(keyring: Option<String>, env: Option<String>) -> Option<String> {
    let present = |v: Option<String>| v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    present(keyring).or_else(|| present(env))
}

pub mod delivery;
pub mod inbound;
