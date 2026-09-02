//! OCR client + two-stage pipeline contract (#939), against a wiremock stand-in
//! for `POST /v1/ocr`. The pipeline tests need `pdftotext` on PATH (poppler);
//! they skip cleanly where it's missing (CI) instead of failing.

use std::path::{Path, PathBuf};

use augmentagent_docs::{extract_text, DocKind, OcrClient, OcrOutcome, DEFAULT_OCR_MODEL};
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn have_pdftotext() -> bool {
    std::process::Command::new("pdftotext")
        .arg("-v")
        .output()
        .is_ok()
}

fn ocr_pages(pages: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "model": DEFAULT_OCR_MODEL,
        "pages": pages.iter().enumerate().map(|(i, md)| serde_json::json!({
            "index": i,
            "markdown": md,
            "images": [],
            "dimensions": {"dpi": 200, "height": 100, "width": 100}
        })).collect::<Vec<_>>(),
        "usage_info": {"pages_processed": pages.len(), "doc_size_bytes": 1234}
    })
}

async fn client_for(server: &MockServer) -> OcrClient {
    OcrClient::new("test-key".into()).with_base_url(server.uri())
}

#[tokio::test]
async fn ocr_pdf_posts_a_bearer_authed_data_uri_and_joins_page_markdown() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/ocr"))
        .and(header("authorization", "Bearer test-key"))
        .and(header("content-type", "application/json"))
        // Model + document shape pinned: Mistral wants the PDF inline as a
        // base64 data URI under `document_url` (no rasterization step).
        .and(body_partial_json(serde_json::json!({
            "model": DEFAULT_OCR_MODEL,
            "document": {
                "type": "document_url",
                "document_url": "data:application/pdf;base64,JVBERi0="
            }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(ocr_pages(&[
            "# Page one\n\nFindings: none",
            "Page two body",
        ])))
        .expect(1)
        .mount(&server)
        .await;

    let client = client_for(&server).await;
    let out = client.ocr_pdf(b"%PDF-").await.expect("ocr ok");
    assert_eq!(out.pages, 2);
    assert_eq!(
        out.markdown,
        "# Page one\n\nFindings: none\n\nPage two body"
    );
}

#[tokio::test]
async fn ocr_pdf_orders_pages_by_index_even_when_the_api_shuffles_them() {
    let server = MockServer::start().await;
    let body = serde_json::json!({
        "pages": [
            {"index": 1, "markdown": "second"},
            {"index": 0, "markdown": "first"}
        ],
        "usage_info": {"pages_processed": 2}
    });
    Mock::given(method("POST"))
        .and(path("/v1/ocr"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;
    let out = client_for(&server).await.ocr_pdf(b"%PDF-").await.unwrap();
    assert_eq!(out.markdown, "first\n\nsecond");
    assert_eq!(out.pages, 2);
}

#[tokio::test]
async fn ocr_pdf_honours_a_model_override() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/ocr"))
        .and(body_partial_json(
            serde_json::json!({"model": "mistral-ocr-4-1"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(ocr_pages(&["x"])))
        .expect(1)
        .mount(&server)
        .await;
    let client = client_for(&server)
        .await
        .with_model("mistral-ocr-4-1".into());
    assert_eq!(client.model(), "mistral-ocr-4-1");
    client.ocr_pdf(b"%PDF-").await.expect("ocr ok");
}

#[tokio::test]
async fn ocr_pdf_surfaces_http_failures_with_status_and_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/ocr"))
        .respond_with(ResponseTemplate::new(401).set_body_string(r#"{"message":"Unauthorized"}"#))
        .mount(&server)
        .await;
    let err = client_for(&server)
        .await
        .ocr_pdf(b"%PDF-")
        .await
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("401"), "{msg}");
    assert!(msg.contains("Unauthorized"), "{msg}");
}

#[tokio::test]
async fn ocr_pdf_rejects_a_response_without_pages() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/ocr"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
        .mount(&server)
        .await;
    let err = client_for(&server)
        .await
        .ocr_pdf(b"%PDF-")
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("pages"), "{err:#}");
}

#[tokio::test]
async fn ocr_pdf_refuses_inputs_over_the_size_cap_without_calling_the_api() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/ocr"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ocr_pages(&["x"])))
        .expect(0)
        .mount(&server)
        .await;
    let client = client_for(&server).await.with_max_bytes(4);
    let err = client.ocr_pdf(b"%PDF-1.4").await.unwrap_err();
    assert!(format!("{err:#}").contains("too large"), "{err:#}");
}

// ---- two-stage pipeline -------------------------------------------------

#[tokio::test]
async fn text_layer_pdf_never_consults_ocr() {
    if !have_pdftotext() {
        eprintln!("skipping: pdftotext not on PATH");
        return;
    }
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/ocr"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ocr_pages(&["SHOULD NOT BE USED"])))
        .expect(0)
        .mount(&server)
        .await;
    let client = client_for(&server).await;

    let out = extract_text(DocKind::Pdf, &fixture("text.pdf"), Some(&client))
        .await
        .expect("extract ok");
    assert_eq!(out.ocr, OcrOutcome::NotNeeded);
    assert!(
        out.text.contains("AUGMENTAGENT TEXT LAYER FIXTURE"),
        "{}",
        out.text
    );
    assert!(!out.text.contains("SHOULD NOT BE USED"));
}

#[tokio::test]
async fn scanned_pdf_falls_back_to_ocr_when_a_client_is_configured() {
    if !have_pdftotext() {
        eprintln!("skipping: pdftotext not on PATH");
        return;
    }
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/ocr"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(ocr_pages(&["SCANNED FIXTURE\n\nno text layer"])),
        )
        .expect(1)
        .mount(&server)
        .await;
    let client = client_for(&server).await;

    let out = extract_text(DocKind::Pdf, &fixture("scanned.pdf"), Some(&client))
        .await
        .expect("extract ok");
    assert_eq!(out.ocr, OcrOutcome::Applied { pages: 1 });
    assert_eq!(out.text, "SCANNED FIXTURE\n\nno text layer");
}

#[tokio::test]
async fn scanned_pdf_without_a_key_degrades_to_an_unavailable_signal_not_an_error() {
    if !have_pdftotext() {
        eprintln!("skipping: pdftotext not on PATH");
        return;
    }
    // Owner rule (#939): no key ⇒ stage 2 is skipped entirely — no HTTP call,
    // no crash, no throw — and the caller gets stage 1's (empty) text plus a
    // clear "OCR unavailable" signal.
    let out = extract_text(DocKind::Pdf, &fixture("scanned.pdf"), None)
        .await
        .expect("must not error");
    assert_eq!(out.ocr, OcrOutcome::Unavailable);
    assert!(
        out.text
            .trim_matches(|c: char| c.is_whitespace())
            .is_empty(),
        "{:?}",
        out.text
    );
    assert!(out.ocr.note().unwrap().contains("MISTRAL_API_KEY"));
}

#[tokio::test]
async fn scanned_pdf_reports_a_failed_ocr_call_without_erroring() {
    if !have_pdftotext() {
        eprintln!("skipping: pdftotext not on PATH");
        return;
    }
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/ocr"))
        .respond_with(ResponseTemplate::new(500).set_body_string("upstream exploded"))
        .mount(&server)
        .await;
    let client = client_for(&server).await;

    let out = extract_text(DocKind::Pdf, &fixture("scanned.pdf"), Some(&client))
        .await
        .expect("a failed OCR call degrades, it does not abort the attachment");
    match &out.ocr {
        OcrOutcome::Failed(reason) => {
            assert!(reason.contains("500"), "{reason}");
            assert!(reason.contains("upstream exploded"), "{reason}");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert!(out.text.trim().is_empty());
}

#[tokio::test]
async fn missing_input_file_is_still_an_error() {
    let err = extract_text(DocKind::Pdf, Path::new("/nonexistent/aa-doc-0-0.pdf"), None)
        .await
        .unwrap_err();
    assert!(!format!("{err:#}").is_empty());
}
