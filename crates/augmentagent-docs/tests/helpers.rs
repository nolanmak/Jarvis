//! Pure-helper contract for the docs crate (#939): kind detection, converter
//! dispatch, the near-empty threshold that decides whether OCR runs, and the
//! human-readable notes that flow into prompts / CLI output.

use std::path::Path;

use augmentagent_docs::{
    doc_command_for, doc_kind_for, resolve_api_key, text_is_near_empty, DocKind, OcrOutcome,
    NEAR_EMPTY_THRESHOLD,
};

#[test]
fn doc_kind_for_detects_each_format_by_mime_or_extension() {
    assert_eq!(
        doc_kind_for("report.pdf", Some("application/pdf")),
        Some(DocKind::Pdf)
    );
    // Discord sometimes omits content_type — extension still detects.
    assert_eq!(doc_kind_for("report.pdf", None), Some(DocKind::Pdf));
    // Case-insensitive extension.
    assert_eq!(doc_kind_for("REPORT.PDF", None), Some(DocKind::Pdf));
    // MIME wins even when the name has no extension.
    assert_eq!(
        doc_kind_for("noext", Some("application/pdf")),
        Some(DocKind::Pdf)
    );
    assert_eq!(
        doc_kind_for(
            "notes.docx",
            Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
        ),
        Some(DocKind::Docx)
    );
    assert_eq!(doc_kind_for("notes.docx", None), Some(DocKind::Docx));
    assert_eq!(
        doc_kind_for("legacy.doc", Some("application/msword")),
        Some(DocKind::Doc)
    );
    assert_eq!(doc_kind_for("legacy.doc", None), Some(DocKind::Doc));
}

#[test]
fn doc_kind_for_returns_none_for_non_docs() {
    assert_eq!(doc_kind_for("a.png", Some("image/png")), None);
    assert_eq!(doc_kind_for("b.txt", Some("text/plain")), None);
    assert_eq!(doc_kind_for("c.zip", Some("application/zip")), None);
    assert_eq!(doc_kind_for("noext", None), None);
}

#[test]
fn doc_kind_mime_round_trips() {
    assert_eq!(DocKind::Pdf.mime(), "application/pdf");
    assert_eq!(
        DocKind::Docx.mime(),
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
    );
    assert_eq!(DocKind::Doc.mime(), "application/msword");
    for k in [DocKind::Pdf, DocKind::Docx, DocKind::Doc] {
        assert_eq!(doc_kind_for("x", Some(k.mime())), Some(k));
    }
}

#[test]
fn doc_command_for_pdf_invokes_pdftotext_to_stdout() {
    let (program, args) = doc_command_for(DocKind::Pdf, Path::new("/tmp/x.pdf"));
    assert_eq!(program, "pdftotext");
    assert!(args.iter().any(|a| a == "-layout"));
    assert!(args.iter().any(|a| a == "/tmp/x.pdf"));
    // Trailing "-" tells pdftotext to write to stdout.
    assert_eq!(args.last().map(String::as_str), Some("-"));
}

#[test]
fn doc_command_for_docx_and_doc_invoke_pandoc() {
    for kind in [DocKind::Docx, DocKind::Doc] {
        let (program, args) = doc_command_for(kind, Path::new("/tmp/x.docx"));
        assert_eq!(program, "pandoc", "kind={kind:?}");
        assert!(args.iter().any(|a| a == "--to=plain"), "kind={kind:?}");
        assert!(args.iter().any(|a| a == "/tmp/x.docx"), "kind={kind:?}");
    }
}

#[test]
fn near_empty_counts_non_whitespace_chars_against_the_threshold() {
    assert!(text_is_near_empty(""));
    // pdftotext emits a lone form feed per page on image-only PDFs.
    assert!(text_is_near_empty("\x0c"));
    assert!(text_is_near_empty("  \n\n\t \x0c\x0c \n"));
    // A stray header fragment is still "no usable text".
    let just_under: String = "x".repeat(NEAR_EMPTY_THRESHOLD - 1);
    assert!(text_is_near_empty(&just_under));
    let at_threshold: String = "x".repeat(NEAR_EMPTY_THRESHOLD);
    assert!(!text_is_near_empty(&at_threshold));
    // Whitespace between the chars doesn't count toward the threshold.
    let spaced = "x ".repeat(NEAR_EMPTY_THRESHOLD - 1);
    assert!(text_is_near_empty(&spaced));
    assert!(!text_is_near_empty(
        "Findings: no active infestation observed. Technician 88213."
    ));
}

#[test]
fn ocr_outcome_notes_signal_only_the_interesting_cases() {
    // Text layer was fine → nothing to say.
    assert_eq!(OcrOutcome::NotNeeded.note(), None);

    let applied = OcrOutcome::Applied { pages: 3 }
        .note()
        .expect("applied has a note");
    assert!(applied.contains("OCR"), "{applied}");
    assert!(applied.contains("3 page"), "{applied}");

    // The #939 owner rule: an unset key must degrade to a CLEAR signal, not a
    // silent blank — name the env var and suggest the screenshot workaround.
    let unavailable = OcrOutcome::Unavailable
        .note()
        .expect("unavailable has a note");
    assert!(unavailable.contains("OCR unavailable"), "{unavailable}");
    assert!(unavailable.contains("MISTRAL_API_KEY"), "{unavailable}");
    assert!(
        unavailable.to_lowercase().contains("screenshot"),
        "{unavailable}"
    );

    let failed = OcrOutcome::Failed("mistral → 500: boom".into())
        .note()
        .expect("failed has a note");
    assert!(failed.contains("OCR failed"), "{failed}");
    assert!(failed.contains("boom"), "{failed}");
}

#[test]
fn ocr_outcome_summary_is_a_short_status_token() {
    assert_eq!(OcrOutcome::NotNeeded.summary(), "not-needed");
    assert_eq!(
        OcrOutcome::Applied { pages: 1 }.summary(),
        "applied (1 page)"
    );
    assert_eq!(
        OcrOutcome::Applied { pages: 3 }.summary(),
        "applied (3 pages)"
    );
    assert_eq!(
        OcrOutcome::Unavailable.summary(),
        "unavailable (MISTRAL_API_KEY not set)"
    );
    assert_eq!(OcrOutcome::Failed("x".into()).summary(), "failed: x");
}

#[test]
fn resolve_api_key_prefers_keyring_then_env_and_ignores_blanks() {
    assert_eq!(
        resolve_api_key(Some("ring".into()), Some("env".into())),
        Some("ring".into())
    );
    assert_eq!(
        resolve_api_key(None, Some("env".into())),
        Some("env".into())
    );
    assert_eq!(resolve_api_key(None, None), None);
    // A blank value in either source counts as unset (a `MISTRAL_API_KEY=`
    // placeholder line in .env must not "enable" OCR with an empty bearer).
    assert_eq!(resolve_api_key(Some("  ".into()), Some("".into())), None);
    assert_eq!(
        resolve_api_key(Some("".into()), Some("env".into())),
        Some("env".into())
    );
    // Surrounding whitespace is trimmed.
    assert_eq!(
        resolve_api_key(None, Some(" k \n".into())),
        Some("k".into())
    );
}
