//! Transport-neutral policy for files the owner sends the agent (#1293).
//!
//! Discord (`augmentagent-approval-discord`, `event_handler.rs`) was the only
//! inbound attachment surface; its type rules, limits, rejection footer and
//! prompt shape lived there. They moved here unchanged so Slack (and later
//! WhatsApp, #1237) feed the reasoner exactly the same representation:
//!
//! - [`classify`] sorts one file into image / text / document or a
//!   [`RejectReason`] from its name, MIME type and declared size.
//! - [`build_prompt`] turns the user's text plus the downloaded files into the
//!   current-turn prompt (`IMAGE:` markers for images, a path list for text
//!   and converted documents).
//! - [`format_rejection_footer`] is the owner-facing "skipped" line.
//! - [`sanitize_filename`] gives a downloaded file a name that is safe and
//!   identical on Linux and macOS.
//!
//! Retention: files live only for the turn, as on Discord today. Longer
//! retention is #995 and must change every surface together.

use std::path::{Path, PathBuf};

use crate::{doc_kind_for, DocKind};

/// Atomically publish an owner-only attachment without following a preexisting
/// destination symlink or modifying another hard link's inode. The temporary
/// file is removed if writing fails or the future is cancelled before publish.
pub async fn write_private_attachment(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let parent = path.parent().ok_or_else(|| std::io::Error::new(
        std::io::ErrorKind::InvalidInput, "attachment requires a parent directory"))?;
    let temporary = tempfile::Builder::new().prefix(".aa-inbound-").tempfile_in(parent)?;
    let mut output = tokio::fs::File::from_std(temporary.as_file().try_clone()?);
    output.write_all(bytes).await?;
    output.flush().await?;
    drop(output);
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

/// Soft cap on how much of a text file goes into the prompt. Larger files are
/// truncated to this many bytes and annotated `TRUNCATED`.
pub const MAX_TEXT_BYTES: u64 = 1_048_576; // 1 MiB

/// Hard cap on a text or document attachment. Larger files are rejected
/// before download.
pub const MAX_DOWNLOAD_BYTES: u64 = 8 * 1_048_576; // 8 MiB

/// Cap on an image download for surfaces that must bound every transfer
/// (Slack streams through an authenticated request). Discord does not apply
/// it: [`classify`] accepts images of any declared size, as it always has.
pub const MAX_IMAGE_BYTES: u64 = 20 * 1_048_576; // 20 MiB

/// Extensions accepted as text even when the MIME type is missing or generic.
#[rustfmt::skip]
pub const TEXT_EXT_ALLOWLIST: &[&str] = &[
    // plain text & docs
    "txt", "md", "markdown", "rst", "log",
    // structured data
    "json", "yaml", "yml", "toml", "csv", "tsv", "xml",
    // source code
    "rs", "ts", "tsx", "js", "jsx", "mjs", "cjs",
    "py", "go", "java", "c", "cc", "cpp", "h", "hpp",
    "cs", "rb", "php", "swift", "kt", "scala",
    "sh", "bash", "zsh", "sql",
    "html", "css", "scss", "less",
    // config
    "ini", "conf", "cfg", "properties",
];

/// Extensions refused even when the MIME type matches: formats that commonly
/// hold credentials.
pub const TEXT_EXT_DENYLIST: &[&str] = &["env", "pem", "key", "p12", "pfx"];

/// What an accepted file is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboundKind {
    /// Passed to the reasoner as an `IMAGE:` marker.
    Image,
    /// Read as text (truncated to [`MAX_TEXT_BYTES`]).
    Text,
    /// Converted to text first (pdftotext / pandoc, OCR for scanned PDFs).
    Doc(DocKind),
}

impl InboundKind {
    /// Largest download a surface that streams files should accept.
    pub fn download_cap(self) -> u64 {
        match self {
            InboundKind::Image => MAX_IMAGE_BYTES,
            InboundKind::Text | InboundKind::Doc(_) => MAX_DOWNLOAD_BYTES,
        }
    }
}

/// Why a file was not handed to the reasoner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectReason {
    /// Larger than `limit` bytes.
    Oversize { size: u64, limit: u64 },
    /// Extension in [`TEXT_EXT_DENYLIST`].
    SecurityDenylist,
    /// Not an image, text or supported document.
    UnsupportedType {
        content_type: Option<String>,
        ext: Option<String>,
    },
    /// Accepted by type but could not be fetched or converted; the reason is
    /// owner-facing and never contains a URL or a token.
    Unavailable(String),
}

/// A file that was skipped, for the owner-facing footer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejected {
    pub filename: String,
    pub reason: RejectReason,
}

fn lower_ext(filename: &str) -> Option<String> {
    Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_lowercase())
}

/// Classify one file. Order matters and matches Discord's historic
/// partition: `image/*` first (no size gate), then the credential denylist,
/// then the size cap, then documents, then text by MIME or extension.
pub fn classify(
    filename: &str,
    content_type: Option<&str>,
    size: u64,
) -> Result<InboundKind, RejectReason> {
    if content_type.is_some_and(|ct| ct.starts_with("image/")) {
        return Ok(InboundKind::Image);
    }
    let ext = lower_ext(filename);
    if ext
        .as_deref()
        .is_some_and(|e| TEXT_EXT_DENYLIST.contains(&e))
    {
        return Err(RejectReason::SecurityDenylist);
    }
    if size > MAX_DOWNLOAD_BYTES {
        return Err(RejectReason::Oversize {
            size,
            limit: MAX_DOWNLOAD_BYTES,
        });
    }
    if let Some(kind) = doc_kind_for(filename, content_type) {
        return Ok(InboundKind::Doc(kind));
    }
    let is_text_mime = content_type.is_some_and(|ct| ct.starts_with("text/"));
    let is_allowlisted_ext = ext
        .as_deref()
        .is_some_and(|e| TEXT_EXT_ALLOWLIST.contains(&e));
    if is_text_mime || is_allowlisted_ext {
        Ok(InboundKind::Text)
    } else {
        Err(RejectReason::UnsupportedType {
            content_type: content_type.map(str::to_string),
            ext,
        })
    }
}

/// Human-readable size: `512 B`, `12 KB`, `9.0 MB`.
pub fn format_size(bytes: u64) -> String {
    const MB: f64 = 1_048_576.0;
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

/// The owner-facing "skipped" line, or `None` when nothing was skipped.
pub fn format_rejection_footer(rejected: &[Rejected]) -> Option<String> {
    if rejected.is_empty() {
        return None;
    }
    let parts: Vec<String> = rejected
        .iter()
        .map(|r| match &r.reason {
            RejectReason::Oversize { size, limit } => format!(
                "{} ({} > {})",
                r.filename,
                format_size(*size),
                format_size(*limit),
            ),
            RejectReason::SecurityDenylist => format!("{} (security)", r.filename),
            RejectReason::UnsupportedType { content_type, ext } => {
                let detail = content_type
                    .clone()
                    .or_else(|| ext.clone())
                    .unwrap_or_else(|| "unknown".to_string());
                format!("{} (unsupported: {})", r.filename, detail)
            }
            RejectReason::Unavailable(why) => format!("{} ({})", r.filename, why),
        })
        .collect();
    Some(format!("\u{26A0}\u{FE0F} skipped: {}", parts.join(", ")))
}

/// Keep at most [`MAX_TEXT_BYTES`]; `true` when something was cut.
pub fn truncate_text_bytes(bytes: &[u8]) -> (&[u8], bool) {
    let cap = MAX_TEXT_BYTES as usize;
    if bytes.len() > cap {
        (&bytes[..cap], true)
    } else {
        (bytes, false)
    }
}

/// A text file (or a converted document) on disk for the reasoner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextAttachment {
    pub path: PathBuf,
    /// Only the first [`MAX_TEXT_BYTES`] were kept.
    pub truncated: bool,
    /// Size before truncation (for documents: of the extracted text).
    pub original_size: u64,
    /// OCR outcome for a converted document (#939); `None` otherwise.
    pub note: Option<String>,
}

/// Combine the user's text (possibly empty) with the downloaded files. The
/// `IMAGE:` marker lines are the cross-provider convention defined in
/// `augmentagent_channel_core::images`; the prefix is a literal here because
/// this crate sits below channel-core.
pub fn build_prompt(user_text: &str, images: &[PathBuf], text_files: &[TextAttachment]) -> String {
    if images.is_empty() && text_files.is_empty() {
        return user_text.to_string();
    }
    let mut s = String::new();
    if !user_text.is_empty() {
        s.push_str(user_text);
        s.push_str("\n\n");
    }
    if !images.is_empty() {
        s.push_str("[attached images to analyze — open each IMAGE path]\n");
        for path in images {
            s.push_str(&format!("IMAGE: {}\n", path.display()));
        }
    }
    if !text_files.is_empty() {
        s.push_str("[attached text files to read]\n");
        for f in text_files {
            s.push_str("- ");
            s.push_str(&f.path.display().to_string());
            if f.truncated {
                s.push_str(&format!(
                    "  (TRUNCATED — first {} of {})",
                    format_size(MAX_TEXT_BYTES),
                    format_size(f.original_size),
                ));
            }
            if let Some(note) = &f.note {
                s.push_str(&format!("  ({note})"));
            }
            s.push('\n');
        }
    }
    s.push_str("\nUse the Read tool to view each attachment and answer based on them.");
    s
}

/// Extension for the file on disk: the name's (lowercased), else from the
/// MIME subtype, else `bin`.
pub fn extension_for(filename: &str, content_type: Option<&str>) -> String {
    if let Some(ext) = Path::new(filename).extension().and_then(|e| e.to_str()) {
        if !ext.is_empty() {
            return ext.to_lowercase();
        }
    }
    if let Some(ct) = content_type {
        if let Some(rest) = ct.strip_prefix("image/") {
            let ext = match rest {
                "jpeg" => "jpg",
                other => other,
            };
            return ext.to_string();
        }
        match ct {
            "text/plain" => return "txt".into(),
            "text/markdown" => return "md".into(),
            "text/csv" => return "csv".into(),
            _ => {}
        }
    }
    "bin".into()
}

/// Longest sanitized stem, in bytes (all ASCII).
const MAX_STEM: usize = 64;
/// Longest kept extension, in bytes.
const MAX_EXT: usize = 10;

/// A single safe path component for the `index`-th file of a message.
///
/// Pure string function, so Linux and macOS produce the same name. Only
/// `[A-Za-z0-9._-]` survive; everything else (separators, spaces, Unicode,
/// control characters) becomes `_`, runs collapse, leading dots go (no
/// hidden files, no `.`/`..`). The two-digit `index` prefix makes names
/// unique within a message regardless of case or Unicode normalisation, so
/// `Report.pdf` and `report.pdf` never collide on a case-insensitive APFS
/// volume. The original name is kept separately for the owner and prompt.
pub fn sanitize_filename(index: usize, original: &str) -> String {
    // Last path component only: `a/b\c.txt` → `c.txt`.
    let base = original.rsplit(['/', '\\']).next().unwrap_or_default();
    let clean = |s: &str| -> String {
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            let keep = c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_');
            let c = if keep { c } else { '_' };
            if c == '_' && out.ends_with('_') {
                continue;
            }
            out.push(c);
        }
        out
    };
    let (stem, ext) = match base.rsplit_once('.') {
        Some((stem, ext)) if !stem.trim_matches('.').is_empty() && !ext.is_empty() => {
            (stem, Some(ext))
        }
        _ => (base, None),
    };
    let mut stem = clean(stem).trim_matches(['.', '_']).to_string();
    stem.truncate(MAX_STEM);
    if stem.is_empty() {
        stem.push_str("file");
    }
    let ext: String = ext
        .map(|e| clean(e).trim_matches(['.', '_']).to_ascii_lowercase())
        .filter(|e| !e.is_empty())
        .map(|mut e| {
            e.truncate(MAX_EXT);
            e
        })
        .unwrap_or_default();
    if ext.is_empty() {
        format!("{index:02}-{stem}")
    } else {
        format!("{index:02}-{stem}.{ext}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_matches_the_discord_partition_rules() {
        // Images first, with no size gate (Discord's historic behaviour).
        assert_eq!(
            classify("photo.png", Some("image/png"), u64::MAX),
            Ok(InboundKind::Image)
        );
        // Credential formats refused even with a text MIME.
        assert_eq!(
            classify("prod.ENV", Some("text/plain"), 10),
            Err(RejectReason::SecurityDenylist)
        );
        // Size gate applies before type detection.
        assert_eq!(
            classify("big.log", Some("text/plain"), MAX_DOWNLOAD_BYTES + 1),
            Err(RejectReason::Oversize {
                size: MAX_DOWNLOAD_BYTES + 1,
                limit: MAX_DOWNLOAD_BYTES
            })
        );
        assert_eq!(
            classify("ok.log", Some("text/plain"), MAX_DOWNLOAD_BYTES),
            Ok(InboundKind::Text)
        );
        assert_eq!(
            classify("r.pdf", None, 10),
            Ok(InboundKind::Doc(DocKind::Pdf))
        );
        assert_eq!(
            classify(
                "x",
                Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
                10
            ),
            Ok(InboundKind::Doc(DocKind::Docx))
        );
        assert_eq!(classify("main.RS", None, 10), Ok(InboundKind::Text));
        assert_eq!(
            classify("notes", Some("text/x-anything"), 10),
            Ok(InboundKind::Text)
        );
        assert_eq!(
            classify("a.zip", Some("application/zip"), 10),
            Err(RejectReason::UnsupportedType {
                content_type: Some("application/zip".into()),
                ext: Some("zip".into())
            })
        );
    }

    #[test]
    fn streaming_caps_bound_images_too() {
        assert_eq!(InboundKind::Image.download_cap(), MAX_IMAGE_BYTES);
        assert_eq!(InboundKind::Text.download_cap(), MAX_DOWNLOAD_BYTES);
        assert_eq!(
            InboundKind::Doc(DocKind::Pdf).download_cap(),
            MAX_DOWNLOAD_BYTES
        );
    }

    #[test]
    fn footer_renders_every_reason() {
        assert_eq!(format_rejection_footer(&[]), None);
        let footer = format_rejection_footer(&[
            Rejected {
                filename: "big.bin".into(),
                reason: RejectReason::Oversize {
                    size: 9 * 1_048_576,
                    limit: MAX_DOWNLOAD_BYTES,
                },
            },
            Rejected {
                filename: "a.pem".into(),
                reason: RejectReason::SecurityDenylist,
            },
            Rejected {
                filename: "a.zip".into(),
                reason: RejectReason::UnsupportedType {
                    content_type: None,
                    ext: Some("zip".into()),
                },
            },
            Rejected {
                filename: "scan.pdf".into(),
                reason: RejectReason::Unavailable("pdftotext is not installed".into()),
            },
        ])
        .unwrap();
        assert_eq!(
            footer,
            "\u{26A0}\u{FE0F} skipped: big.bin (9.0 MB > 8.0 MB), a.pem (security), \
             a.zip (unsupported: zip), scan.pdf (pdftotext is not installed)"
        );
    }

    #[test]
    fn prompt_shape_is_the_discord_one() {
        assert_eq!(build_prompt("hi", &[], &[]), "hi");
        let prompt = build_prompt(
            "",
            &[PathBuf::from("/s/00-a.png")],
            &[TextAttachment {
                path: PathBuf::from("/s/01-log.txt"),
                truncated: true,
                original_size: 2 * 1_048_576,
                note: Some("ocr note".into()),
            }],
        );
        assert_eq!(
            prompt,
            "[attached images to analyze — open each IMAGE path]\nIMAGE: /s/00-a.png\n\
             [attached text files to read]\n- /s/01-log.txt  (TRUNCATED — first 1.0 MB of 2.0 MB)  (ocr note)\n\
             \nUse the Read tool to view each attachment and answer based on them."
        );
    }

    #[test]
    fn truncation_and_extension_rules() {
        let bytes = vec![0u8; MAX_TEXT_BYTES as usize + 1];
        let (slice, cut) = truncate_text_bytes(&bytes);
        assert_eq!((slice.len() as u64, cut), (MAX_TEXT_BYTES, true));
        assert_eq!(truncate_text_bytes(b"abc"), (&b"abc"[..], false));
        assert_eq!(extension_for("photo.JPG", Some("image/jpeg")), "jpg");
        assert_eq!(extension_for("noext", Some("image/jpeg")), "jpg");
        assert_eq!(extension_for("noext", Some("text/markdown")), "md");
        assert_eq!(extension_for("noext", None), "bin");
    }

    #[test]
    fn sanitized_names_are_safe_single_components() {
        let cases = [
            ("report.pdf", "00-report.pdf"),
            ("Q3 report – final.PDF", "00-Q3_report_final.pdf"),
            ("../../etc/passwd", "00-passwd"),
            ("..", "00-file"),
            (".", "00-file"),
            ("", "00-file"),
            (".env", "00-env"),
            ("a/b\\c.txt", "00-c.txt"),
            ("日本語.txt", "00-file.txt"),
            ("naïve café.md", "00-na_ve_caf.md"),
            ("line\nbreak\u{0}.txt", "00-line_break.txt"),
            ("archive.tar.gz", "00-archive.tar.gz"),
            ("..hidden..txt", "00-hidden.txt"),
        ];
        for (input, want) in cases {
            let got = sanitize_filename(0, input);
            assert_eq!(got, want, "{input:?}");
            assert!(!got.contains(['/', '\\']) && got != "." && got != "..");
            assert!(got
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)));
        }
        let long = format!("{}.{}", "x".repeat(500), "y".repeat(50));
        let got = sanitize_filename(3, &long);
        assert!(got.len() <= 3 + MAX_STEM + 1 + MAX_EXT, "{got}");
    }

    #[test]
    fn names_differing_only_by_case_or_unicode_never_collide() {
        // A case-insensitive volume (default APFS) folds these together; the
        // index prefix keeps them apart on every host.
        let names = [
            "Report.pdf",
            "report.pdf",
            "REPORT.PDF",
            "résumé.pdf",
            "resume.pdf",
        ];
        let sanitized: Vec<String> = names
            .iter()
            .enumerate()
            .map(|(i, n)| sanitize_filename(i, n).to_lowercase())
            .collect();
        let unique: std::collections::HashSet<_> = sanitized.iter().collect();
        assert_eq!(unique.len(), names.len(), "{sanitized:?}");
        // And on this host's real filesystem: each lands as its own file.
        let dir = tempfile::tempdir().unwrap();
        for (i, n) in names.iter().enumerate() {
            let p = dir.path().join(sanitize_filename(i, n));
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&p)
                .unwrap_or_else(|e| panic!("{}: {e}", p.display()));
        }
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), names.len());
    }
}

/// #1629 — the human filename behind an opaque `/tmp/aa-{doc,txt,img}-<n>-<i>.<ext>`
/// download. Those paths have to stay opaque (the ask agent's Read carve-out
/// admits only that shape), but re-attaching one to an email must not ship
/// `aa-doc-1875…-0.pdf` to the recipient. Each writer records the original
/// name beside the file; `gmail compose --attach` reads it back.
pub fn original_name_sidecar(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".name");
    PathBuf::from(name)
}

/// Is `path` one of the opaque inbound download names (`aa-<kind>-<n>-<i>.<ext>`)?
pub fn is_opaque_download(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else { return false };
    let Some(rest) = ["aa-doc-", "aa-txt-", "aa-img-"].iter().find_map(|p| name.strip_prefix(p)) else {
        return false;
    };
    let Some((digits, ext)) = rest.split_once('.') else { return false };
    let Some((a, b)) = digits.split_once('-') else { return false };
    [a, b].iter().all(|s| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit()))
        && !ext.is_empty()
        && ext.bytes().all(|c| c.is_ascii_alphanumeric())
}

/// The original filename made safe to put in a MIME header while staying
/// recognisable: last path component only, control characters and quotes
/// dropped, at most 200 characters. `None` when nothing usable is left.
pub fn display_filename(original: &str) -> Option<String> {
    let base = original.rsplit(['/', '\\']).next().unwrap_or_default();
    let cleaned: String = base
        .chars()
        .filter(|c| !c.is_control() && *c != '"')
        .take(200)
        .collect();
    let cleaned = cleaned.trim().to_string();
    (!cleaned.is_empty() && cleaned != "." && cleaned != "..").then_some(cleaned)
}

/// Record `original` as the display name of the opaque download at `path`.
pub async fn record_original_name(path: &Path, original: &str) -> std::io::Result<()> {
    let Some(name) = display_filename(original) else { return Ok(()) };
    write_private_attachment(&original_name_sidecar(path), name.as_bytes()).await
}

/// The recorded display name for an opaque download, if one is trustworthy:
/// a regular owner-only file of ours (never a symlink — `/tmp` names are
/// predictable), no older than the download itself.
pub fn recorded_original_name(path: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    if !is_opaque_download(path) {
        return None;
    }
    let sidecar = original_name_sidecar(path);
    let meta = std::fs::symlink_metadata(&sidecar).ok()?;
    let file = std::fs::metadata(path).ok()?;
    let ours = meta.file_type().is_file()
        && meta.nlink() == 1
        && meta.uid() == file.uid()
        && meta.mode() & 0o077 == 0
        && meta.len() <= 1024
        && meta.modified().ok()? >= file.modified().ok()?;
    if !ours {
        return None;
    }
    display_filename(std::fs::read_to_string(&sidecar).ok()?.trim())
}

#[cfg(test)]
mod original_name_tests {
    use super::*;

    #[test]
    fn only_the_opaque_inbound_shapes_qualify() {
        for ok in ["/tmp/aa-doc-1875167429129085044-0.pdf", "/tmp/aa-txt-1-2.md", "/tmp/aa-img-9-0.PNG"] {
            assert!(is_opaque_download(Path::new(ok)), "{ok}");
        }
        for no in ["/tmp/Proposal.pdf", "/tmp/aa-doc-1-0", "/tmp/aa-doc-1-0.pdf.name", "/tmp/aa-pdf-1-0.pdf",
                   "/tmp/aa-doc-x-0.pdf", "/tmp/aa-doc--0.pdf", "/tmp/aa-doc-1-0.p df"] {
            assert!(!is_opaque_download(Path::new(no)), "{no}");
        }
    }

    #[test]
    fn display_names_keep_the_human_name_but_not_paths_or_header_breakers() {
        assert_eq!(display_filename("Acme - Audit Summary (Plain Language).pdf").as_deref(),
                   Some("Acme - Audit Summary (Plain Language).pdf"));
        assert_eq!(display_filename("../../etc/Q3 \"final\".pdf").as_deref(), Some("Q3 final.pdf"));
        assert_eq!(display_filename("a\r\nBcc: injected.pdf").as_deref(), Some("aBcc: injected.pdf"));
        assert_eq!(display_filename("Résumé.docx").as_deref(), Some("Résumé.docx"));
        assert!(display_filename("  ").is_none() && display_filename("dir/").is_none() && display_filename("..").is_none());
    }

    #[tokio::test]
    async fn a_recorded_name_round_trips_and_untrusted_sidecars_are_ignored() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("aa-doc-12-0.pdf");
        write_private_attachment(&file, b"%PDF-1.7").await.unwrap();
        assert_eq!(recorded_original_name(&file), None, "no sidecar yet");
        record_original_name(&file, "Quarterly Report.pdf").await.unwrap();
        assert_eq!(recorded_original_name(&file).as_deref(), Some("Quarterly Report.pdf"));

        // Group/world-writable sidecar: someone else could have written it.
        let sidecar = original_name_sidecar(&file);
        std::fs::set_permissions(&sidecar, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert_eq!(recorded_original_name(&file), None);

        // A symlink planted at the sidecar path is never followed.
        std::fs::remove_file(&sidecar).unwrap();
        let elsewhere = dir.path().join("planted");
        std::fs::write(&elsewhere, "Invoice.pdf").unwrap();
        std::os::unix::fs::symlink(&elsewhere, &sidecar).unwrap();
        assert_eq!(recorded_original_name(&file), None);

        // A download rewritten after its sidecar makes the old name stale.
        std::fs::remove_file(&sidecar).unwrap();
        record_original_name(&file, "Old.pdf").await.unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::options().write(true).open(&file).unwrap().set_modified(later).unwrap();
        assert_eq!(recorded_original_name(&file), None);

        // Ordinary files are never renamed.
        let plain = dir.path().join("Proposal.pdf");
        std::fs::write(&plain, b"x").unwrap();
        assert_eq!(recorded_original_name(&plain), None);
    }
}
