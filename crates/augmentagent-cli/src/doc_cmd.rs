//! `augmentagent doc extract` (#939): run the shared document → text pipeline
//! on a local file and report whether OCR ran. This is the operator / QA
//! surface for the pipeline the Discord handler and `gmail get-attachment`
//! use internally — same crate, same two stages, same outcome vocabulary.

use std::path::{Path, PathBuf};

use anyhow::Context;
use augmentagent_docs::{doc_kind_for, extract_text, DocKind, OcrClient, OcrOutcome};

/// `<input>.txt` beside the input (so `/tmp/aa-doc-1-0.pdf` → `/tmp/aa-doc-1-0.txt`,
/// which keeps the extracted text inside the ask agent's `/tmp/aa-doc-*` Read
/// carve-out).
pub fn default_out_path(input: &Path) -> PathBuf {
    input.with_extension("txt")
}

/// Resolve the doc kind from an explicit `--kind` or the filename. An explicit
/// flag wins; an unknown flag value or an undetectable filename is an error
/// that names the accepted kinds.
pub fn parse_kind(flag: Option<&str>, input: &Path) -> anyhow::Result<DocKind> {
    if let Some(k) = flag {
        return match k.to_ascii_lowercase().as_str() {
            "pdf" => Ok(DocKind::Pdf),
            "docx" => Ok(DocKind::Docx),
            "doc" => Ok(DocKind::Doc),
            other => anyhow::bail!("--kind {other:?} is not one of pdf, docx, doc"),
        };
    }
    let name = input
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    doc_kind_for(name, None).ok_or_else(|| {
        anyhow::anyhow!(
            "cannot tell the document kind from {:?}; pass --kind pdf|docx|doc",
            input.display()
        )
    })
}

/// One-line human receipt. `out` is `None` when the text went to stdout.
pub fn human_line(out: Option<&Path>, chars: usize, outcome: &OcrOutcome) -> String {
    let target = out
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "stdout".to_string());
    let mut line = format!(
        "extracted: {target} ({chars} chars, ocr: {})",
        outcome.summary()
    );
    if let Some(note) = outcome.note() {
        line.push_str("\nnote: ");
        line.push_str(&note);
    }
    line
}

/// Machine receipt for `--json`.
pub fn receipt_json(
    input: &Path,
    out: Option<&Path>,
    chars: usize,
    outcome: &OcrOutcome,
) -> serde_json::Value {
    serde_json::json!({
        "input": input.display().to_string(),
        "out": out.map(|p| p.display().to_string()),
        "chars": chars,
        "ocr": outcome.summary(),
        "ocr_applied": matches!(outcome, OcrOutcome::Applied { .. }),
        "note": outcome.note(),
    })
}

pub async fn run_doc_extract(
    file: PathBuf,
    out: Option<String>,
    kind: Option<String>,
    no_ocr: bool,
    json: bool,
) -> anyhow::Result<()> {
    let kind = parse_kind(kind.as_deref(), &file)?;
    let ocr = if no_ocr { None } else { OcrClient::from_env() };
    let extracted = extract_text(kind, &file, ocr.as_ref())
        .await
        .with_context(|| format!("extract {}", file.display()))?;
    let chars = extracted.text.chars().count();

    let out_path: Option<PathBuf> = match out.as_deref() {
        Some("-") => None,
        Some(p) => Some(PathBuf::from(p)),
        None => Some(default_out_path(&file)),
    };
    if let Some(p) = &out_path {
        tokio::fs::write(p, extracted.text.as_bytes())
            .await
            .with_context(|| format!("write {}", p.display()))?;
    }

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&receipt_json(
                &file,
                out_path.as_deref(),
                chars,
                &extracted.ocr
            ))?
        );
    } else {
        println!("{}", human_line(out_path.as_deref(), chars, &extracted.ocr));
    }
    if out_path.is_none() {
        // Text to stdout AFTER the receipt so a caller can `tail -n +2`.
        println!("{}", extracted.text);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_out_path_swaps_the_extension_for_txt() {
        assert_eq!(
            default_out_path(Path::new("/tmp/aa-doc-12-0.pdf")),
            PathBuf::from("/tmp/aa-doc-12-0.txt")
        );
        assert_eq!(
            default_out_path(Path::new("/x/PCT REPORTS .pdf")),
            PathBuf::from("/x/PCT REPORTS .txt")
        );
        // No extension → still gets .txt appended.
        assert_eq!(
            default_out_path(Path::new("/x/report")),
            PathBuf::from("/x/report.txt")
        );
    }

    #[test]
    fn parse_kind_prefers_the_flag_then_the_filename() {
        assert_eq!(
            parse_kind(Some("PDF"), Path::new("x.bin")).unwrap(),
            DocKind::Pdf
        );
        assert_eq!(
            parse_kind(Some("docx"), Path::new("x")).unwrap(),
            DocKind::Docx
        );
        assert_eq!(
            parse_kind(Some("doc"), Path::new("x")).unwrap(),
            DocKind::Doc
        );
        assert_eq!(
            parse_kind(None, Path::new("/tmp/a.PDF")).unwrap(),
            DocKind::Pdf
        );
        let err = parse_kind(Some("xls"), Path::new("x"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("pdf, docx, doc"), "{err}");
        let err = parse_kind(None, Path::new("/tmp/mystery.bin"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("--kind"), "{err}");
    }

    #[test]
    fn human_line_carries_the_ocr_status_and_note() {
        let l = human_line(Some(Path::new("/tmp/a.txt")), 120, &OcrOutcome::NotNeeded);
        assert_eq!(l, "extracted: /tmp/a.txt (120 chars, ocr: not-needed)");

        let l = human_line(None, 0, &OcrOutcome::Unavailable);
        assert!(
            l.starts_with("extracted: stdout (0 chars, ocr: unavailable"),
            "{l}"
        );
        assert!(l.contains("\nnote: OCR unavailable"), "{l}");
        assert!(l.contains("MISTRAL_API_KEY"), "{l}");

        let l = human_line(
            Some(Path::new("/tmp/b.txt")),
            900,
            &OcrOutcome::Applied { pages: 3 },
        );
        assert!(l.contains("ocr: applied (3 pages)"), "{l}");
        assert!(l.contains("\nnote: text recovered via OCR"), "{l}");
    }

    #[test]
    fn receipt_json_is_flat_and_typed() {
        let v = receipt_json(
            Path::new("/tmp/in.pdf"),
            Some(Path::new("/tmp/in.txt")),
            42,
            &OcrOutcome::Applied { pages: 2 },
        );
        assert_eq!(v["input"], "/tmp/in.pdf");
        assert_eq!(v["out"], "/tmp/in.txt");
        assert_eq!(v["chars"], 42);
        assert_eq!(v["ocr"], "applied (2 pages)");
        assert_eq!(v["ocr_applied"], true);
        assert!(v["note"].as_str().unwrap().contains("OCR"));

        let v = receipt_json(Path::new("/tmp/in.pdf"), None, 0, &OcrOutcome::NotNeeded);
        assert!(v["out"].is_null());
        assert_eq!(v["ocr_applied"], false);
        assert!(v["note"].is_null());
    }
}
