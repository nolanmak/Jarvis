//! #1293 — owner-sent Slack files through the inbound pipeline into the
//! same transport-neutral turn Discord builds (`augmentagent_docs::inbound`).
//!
//! Files are served by the recording fake or a loopback file host; PDF/DOCX
//! conversion uses tiny fake `pdftotext`/`pandoc` scripts so the tests are
//! identical on Linux and macOS. Fixture media is synthetic and a few bytes.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use augmentagent_channel_slack::inbound::{
    default_inbound_root, prepare_inbound, InboundError, InboundOptions, MAX_FILES_PER_MESSAGE,
};
use augmentagent_channel_slack::transport::event::FileRef;
use augmentagent_channel_slack::transport::token::BotToken;
use augmentagent_channel_slack::transport::web::{
    DownloadLimits, HttpSlackWebApi, RecordedCall, RecordingSlackWebApi, WebApiConfig,
};
use augmentagent_docs::inbound::{InboundKind, RejectReason, MAX_DOWNLOAD_BYTES, MAX_TEXT_BYTES};
use augmentagent_docs::{ConvertOptions, DocKind};
use tokio_util::sync::CancellationToken;

/// Smallest valid PNG: 1x1 transparent pixel.
const PNG_1X1: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];
const PDF: &[u8] = b"%PDF-1.4\n% synthetic\n%%EOF\n";
/// A DOCX is a zip; the fake pandoc only needs the bytes to exist.
const DOCX: &[u8] = b"PK\x03\x04synthetic-docx";

fn url(id: &str, name: &str) -> String {
    format!("https://files.slack.com/files-pri/T00000001-{id}/download/{name}")
}

fn file(id: &str, name: &str, mimetype: &str, size: u64) -> FileRef {
    FileRef {
        id: id.into(),
        name: Some(name.into()),
        mimetype: Some(mimetype.into()),
        url_private: Some(url(id, "private")),
        size: Some(size),
        title: None,
        filetype: None,
        url_private_download: Some(url(id, name)),
        mode: Some("hosted".into()),
        file_access: None,
        is_external: false,
        subtype: None,
        media_display_type: None,
        duration_ms: None,
    }
}

struct Fixture {
    _state: tempfile::TempDir,
    root: PathBuf,
    _tools: tempfile::TempDir,
    opts: InboundOptions,
    api: RecordingSlackWebApi,
}

fn fake_tool(dir: &Path, name: &str, script: &str) {
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{script}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn fixture() -> Fixture {
    // State dir path with spaces and Unicode, like a macOS home can have.
    let state = tempfile::tempdir().unwrap();
    let root = state.path().join("state dir ü").join("slack-inbound");
    let tools = tempfile::tempdir().unwrap();
    fake_tool(
        tools.path(),
        "pdftotext",
        "echo 'PDF TEXT LAYER from synthetic report'",
    );
    fake_tool(
        tools.path(),
        "pandoc",
        "echo 'DOCX BODY from synthetic memo'",
    );
    let mut opts = InboundOptions::new(root.clone());
    opts.convert = ConvertOptions {
        search_path: Some(tools.path().as_os_str().to_owned()),
        fallback_dirs: vec![],
        timeout: Duration::from_secs(5),
        ..ConvertOptions::default()
    };
    Fixture {
        _state: state,
        root,
        _tools: tools,
        opts,
        api: RecordingSlackWebApi::default(),
    }
}

fn entries(dir: &Path) -> Vec<PathBuf> {
    match std::fs::read_dir(dir) {
        Ok(rd) => rd.map(|e| e.unwrap().path()).collect(),
        Err(_) => Vec::new(),
    }
}

fn mode(p: &Path) -> u32 {
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

#[tokio::test]
async fn image_text_pdf_and_docx_reach_the_prompt_like_discord() {
    let fx = fixture();
    let files = vec![
        file("F1", "photo.png", "image/png", PNG_1X1.len() as u64),
        file("F2", "notes.md", "text/markdown", 11),
        file("F3", "Report Q3.pdf", "application/pdf", PDF.len() as u64),
        file(
            "F4",
            "memo.docx",
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            DOCX.len() as u64,
        ),
    ];
    fx.api.add_file(&url("F1", "photo.png"), PNG_1X1.to_vec());
    fx.api
        .add_file(&url("F2", "notes.md"), b"# synthetic".to_vec());
    fx.api.add_file(&url("F3", "Report Q3.pdf"), PDF.to_vec());
    fx.api.add_file(&url("F4", "memo.docx"), DOCX.to_vec());

    let msg = prepare_inbound(
        &fx.api,
        "what do these say?",
        &files,
        &fx.opts,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(msg.starts_turn());
    assert!(msg.rejected.is_empty(), "{:?}", msg.rejected);
    assert_eq!(msg.rejection_notice(), None);
    assert_eq!(msg.images.len(), 1);
    assert_eq!(msg.text_files.len(), 3);
    let kinds: Vec<InboundKind> = msg.accepted.iter().map(|a| a.kind).collect();
    assert_eq!(
        kinds,
        vec![
            InboundKind::Image,
            InboundKind::Text,
            InboundKind::Doc(DocKind::Pdf),
            InboundKind::Doc(DocKind::Docx)
        ]
    );
    // Same prompt the Discord pipeline builds from the same pieces.
    assert_eq!(
        msg.prompt,
        augmentagent_docs::inbound::build_prompt(
            "what do these say?",
            &msg.images,
            &msg.text_files
        )
    );
    assert!(msg.prompt.starts_with("what do these say?\n\n"));
    assert!(msg
        .prompt
        .contains(&format!("IMAGE: {}", msg.images[0].display())));
    assert_eq!(std::fs::read(&msg.images[0]).unwrap(), PNG_1X1);
    assert_eq!(
        std::fs::read_to_string(&msg.text_files[0].path).unwrap(),
        "# synthetic"
    );
    assert!(std::fs::read_to_string(&msg.text_files[1].path)
        .unwrap()
        .contains("PDF TEXT LAYER"));
    assert!(std::fs::read_to_string(&msg.text_files[2].path)
        .unwrap()
        .contains("DOCX BODY"));
    // Private: the message dir is 0700 under the root, files 0600, and the
    // converted documents' originals are gone.
    let dir = msg.dir().unwrap().to_path_buf();
    assert!(dir.starts_with(&fx.root));
    assert_eq!(mode(&fx.root), 0o700);
    assert_eq!(mode(&dir), 0o700);
    for p in entries(&dir) {
        assert_eq!(mode(&p), 0o600, "{}", p.display());
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        assert!(
            !name.ends_with(".pdf") && !name.ends_with(".docx"),
            "{name}"
        );
    }
    assert_eq!(entries(&dir).len(), 4);
    msg.cleanup().unwrap();
    assert!(!dir.exists());
    assert!(entries(&fx.root).is_empty());
}

#[tokio::test]
async fn attachment_only_message_starts_a_turn() {
    let fx = fixture();
    let files = vec![file("F1", "shot.png", "image/png", PNG_1X1.len() as u64)];
    fx.api.add_file(&url("F1", "shot.png"), PNG_1X1.to_vec());
    let msg = prepare_inbound(&fx.api, "   ", &files, &fx.opts, &CancellationToken::new())
        .await
        .unwrap();
    assert!(msg.starts_turn());
    assert_eq!(msg.user_text, "");
    assert!(
        msg.prompt.starts_with("[attached images to analyze"),
        "{}",
        msg.prompt
    );
    drop(msg);
    assert!(entries(&fx.root).is_empty(), "dropped message left files");
}

#[tokio::test]
async fn text_only_message_downloads_nothing_and_creates_no_dir() {
    let fx = fixture();
    let msg = prepare_inbound(&fx.api, "hello", &[], &fx.opts, &CancellationToken::new())
        .await
        .unwrap();
    assert!(msg.starts_turn());
    assert_eq!(msg.prompt, "hello");
    assert!(msg.dir().is_none());
    assert!(fx.api.calls().is_empty());
}

#[tokio::test]
async fn oversized_and_unsupported_files_are_rejected_before_download() {
    let fx = fixture();
    let files = vec![
        file("F1", "huge.log", "text/plain", MAX_DOWNLOAD_BYTES + 1),
        file("F2", "big.png", "image/png", 21 * 1024 * 1024),
        file("F3", "bundle.zip", "application/zip", 10),
        file("F4", "prod.env", "text/plain", 10),
    ];
    let msg = prepare_inbound(&fx.api, "", &files, &fx.opts, &CancellationToken::new())
        .await
        .unwrap();
    assert!(
        !msg.starts_turn(),
        "a wholly rejected empty message must not start a turn"
    );
    assert!(fx.api.calls().is_empty(), "rejected files were downloaded");
    let reasons: Vec<&RejectReason> = msg.rejected.iter().map(|r| &r.reason).collect();
    assert!(matches!(reasons[0], RejectReason::Oversize { .. }));
    assert!(
        matches!(reasons[1], RejectReason::Oversize { limit, .. } if *limit == 20 * 1024 * 1024)
    );
    assert!(matches!(reasons[2], RejectReason::UnsupportedType { .. }));
    assert_eq!(reasons[3], &RejectReason::SecurityDenylist);
    let notice = msg.rejection_notice().unwrap();
    assert_eq!(
        notice,
        "\u{26A0}\u{FE0F} skipped: huge.log (8.0 MB > 8.0 MB), big.png (21.0 MB > 20.0 MB), \
         bundle.zip (unsupported: application/zip), prod.env (security)"
    );
    assert!(entries(&fx.root).is_empty());
}

#[tokio::test]
async fn a_file_larger_than_declared_is_stopped_while_streaming() {
    let fx = fixture();
    // Slack says 10 bytes; the host sends more than the text cap allows.
    let files = vec![file("F1", "liar.txt", "text/plain", 10)];
    fx.api.add_file(
        &url("F1", "liar.txt"),
        vec![b'a'; MAX_DOWNLOAD_BYTES as usize + 1],
    );
    let msg = prepare_inbound(
        &fx.api,
        "check",
        &files,
        &fx.opts,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(msg.text_files.is_empty());
    assert!(matches!(
        msg.rejected[0].reason,
        RejectReason::Oversize { .. }
    ));
    assert!(msg.starts_turn(), "the text still goes to the reasoner");
    assert!(msg.rejection_notice().unwrap().contains("liar.txt"));
    assert!(entries(msg.dir().unwrap()).is_empty(), "partial file kept");
}

#[tokio::test]
async fn long_text_is_truncated_and_annotated() {
    let fx = fixture();
    let size = MAX_TEXT_BYTES as usize + 100;
    let files = vec![file("F1", "big.log", "text/plain", size as u64)];
    fx.api.add_file(&url("F1", "big.log"), vec![b'z'; size]);
    let msg = prepare_inbound(&fx.api, "", &files, &fx.opts, &CancellationToken::new())
        .await
        .unwrap();
    let t = &msg.text_files[0];
    assert!(t.truncated);
    assert_eq!(t.original_size, size as u64);
    assert_eq!(std::fs::metadata(&t.path).unwrap().len(), MAX_TEXT_BYTES);
    assert!(msg.prompt.contains("TRUNCATED"));
}

#[tokio::test]
async fn unavailable_files_get_an_owner_facing_reason() {
    let fx = fixture();
    let mut tomb = file("F1", "gone.txt", "text/plain", 1);
    tomb.mode = Some("tombstone".into());
    let mut ext = file("F2", "linked.pdf", "application/pdf", 1);
    ext.is_external = true;
    ext.mode = Some("external".into());
    let mut connect = file("F3", "shared.txt", "text/plain", 1);
    connect.file_access = Some("check_file_info".into());
    let mut no_url = file("F4", "nourl.txt", "text/plain", 1);
    no_url.url_private = None;
    no_url.url_private_download = None;
    let missing = file("F5", "404.txt", "text/plain", 1); // not served
    let msg = prepare_inbound(
        &fx.api,
        "",
        &[tomb, ext, connect, no_url, missing],
        &fx.opts,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(!msg.starts_turn());
    let notice = msg.rejection_notice().unwrap();
    for (name, why) in [
        ("gone.txt", "deleted"),
        ("linked.pdf", "outside Slack"),
        ("shared.txt", "Slack Connect"),
        ("nourl.txt", "no download link"),
        ("404.txt", "couldn't download"),
    ] {
        assert!(
            notice.contains(name) && notice.contains(why),
            "{name}/{why}: {notice}"
        );
    }
    assert!(!notice.contains("https://"), "URL leaked: {notice}");
    // Only the one with a URL and no availability problem was fetched.
    assert_eq!(fx.api.calls().len(), 1);
}

#[tokio::test]
async fn missing_converter_is_a_clear_rejection_and_the_rest_still_arrives() {
    let mut fx = fixture();
    let empty = tempfile::tempdir().unwrap();
    fx.opts.convert.search_path = Some(empty.path().as_os_str().to_owned());
    let files = vec![
        file("F1", "report.pdf", "application/pdf", PDF.len() as u64),
        file("F2", "a.txt", "text/plain", 2),
    ];
    fx.api.add_file(&url("F1", "report.pdf"), PDF.to_vec());
    fx.api.add_file(&url("F2", "a.txt"), b"ok".to_vec());
    let msg = prepare_inbound(&fx.api, "", &files, &fx.opts, &CancellationToken::new())
        .await
        .unwrap();
    assert!(msg.starts_turn());
    assert_eq!(msg.text_files.len(), 1);
    let notice = msg.rejection_notice().unwrap();
    assert!(
        notice.contains("report.pdf") && notice.contains("pdftotext is not installed"),
        "{notice}"
    );
    // The downloaded original was removed with the failed conversion.
    assert_eq!(entries(msg.dir().unwrap()).len(), 1);
}

#[tokio::test]
async fn names_that_differ_only_by_case_or_are_hostile_do_not_collide_or_escape() {
    let fx = fixture();
    let names = [
        "Report.txt",
        "report.txt",
        "../../escape.txt",
        "日本 語.txt",
        "..",
    ];
    let files: Vec<FileRef> = names
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let id = format!("F{i}");
            fx.api.add_file(
                &url(&id, &format!("n{i}")),
                format!("body {i}").into_bytes(),
            );
            let mut f = file(&id, n, "text/plain", 6);
            f.url_private_download = Some(url(&id, &format!("n{i}")));
            f
        })
        .collect();
    let msg = prepare_inbound(&fx.api, "", &files, &fx.opts, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(msg.text_files.len(), names.len(), "{:?}", msg.rejected);
    let dir = msg.dir().unwrap();
    for (i, t) in msg.text_files.iter().enumerate() {
        assert_eq!(t.path.parent().unwrap(), dir, "{}", t.path.display());
        assert_eq!(
            std::fs::read_to_string(&t.path).unwrap(),
            format!("body {i}")
        );
    }
    // Original names are kept for the owner-facing summary.
    assert_eq!(msg.accepted[2].original_name, "../../escape.txt");
    assert_eq!(entries(dir).len(), names.len());
}

#[tokio::test]
async fn too_many_files_are_rejected_past_the_limit() {
    let fx = fixture();
    let files: Vec<FileRef> = (0..MAX_FILES_PER_MESSAGE + 2)
        .map(|i| {
            let id = format!("F{i}");
            fx.api.add_file(&url(&id, "a.txt"), b"x".to_vec());
            file(&id, "a.txt", "text/plain", 1)
        })
        .collect();
    let msg = prepare_inbound(&fx.api, "", &files, &fx.opts, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(msg.text_files.len(), MAX_FILES_PER_MESSAGE);
    assert_eq!(msg.rejected.len(), 2);
    assert!(msg
        .rejection_notice()
        .unwrap()
        .contains("files per message"));
}

/// A loopback file host that sends headers, then stalls.
async fn stalling_host() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        while let Ok((mut sock, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let _ = sock
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 1000\r\n\r\npart")
                    .await;
                tokio::time::sleep(Duration::from_secs(30)).await;
            });
        }
    });
    (format!("127.0.0.1:{}", addr.port()), task)
}

fn http_api(host: &str, timeout: Duration) -> HttpSlackWebApi {
    HttpSlackWebApi::new(BotToken::new("xoxb-test-000"), WebApiConfig::default())
        .unwrap()
        .with_download_limits(DownloadLimits {
            allowed_hosts: vec![host.to_string()],
            transfer_timeout: timeout,
            max_redirects: 3,
        })
}

#[tokio::test]
async fn a_download_timeout_is_reported_and_cleaned_up() {
    let fx = fixture();
    let (host, task) = stalling_host().await;
    let api = http_api(&host, Duration::from_millis(300));
    let mut f = file("F1", "slow.txt", "text/plain", 1000);
    f.url_private_download = Some(format!("http://{host}/files-pri/slow"));
    let msg = prepare_inbound(&api, "", &[f], &fx.opts, &CancellationToken::new())
        .await
        .unwrap();
    assert!(!msg.starts_turn());
    assert!(msg.rejection_notice().unwrap().contains("timed out"));
    assert!(entries(msg.dir().unwrap()).is_empty());
    drop(msg);
    assert!(entries(&fx.root).is_empty());
    task.abort();
}

#[tokio::test]
async fn cancellation_mid_download_returns_cancelled_and_leaves_nothing() {
    let fx = fixture();
    let (host, task) = stalling_host().await;
    let api = http_api(&host, Duration::from_secs(30));
    fx.api.add_file(&url("F0", "a.txt"), b"ok".to_vec());
    let mut f = file("F1", "slow.txt", "text/plain", 1000);
    f.url_private_download = Some(format!("http://{host}/files-pri/slow"));
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        trigger.cancel();
    });
    let err = prepare_inbound(&api, "hi", &[f], &fx.opts, &cancel)
        .await
        .unwrap_err();
    assert!(matches!(err, InboundError::Cancelled), "{err:?}");
    assert!(entries(&fx.root).is_empty(), "cancelled turn left files");
    task.abort();
}

#[tokio::test]
async fn a_stuck_converter_is_bounded_and_cleaned_up() {
    let mut fx = fixture();
    let tools = tempfile::tempdir().unwrap();
    fake_tool(tools.path(), "pdftotext", "sleep 10");
    fx.opts.convert.search_path = Some(tools.path().as_os_str().to_owned());
    fx.opts.convert.timeout = Duration::from_millis(300);
    let files = vec![file("F1", "r.pdf", "application/pdf", PDF.len() as u64)];
    fx.api.add_file(&url("F1", "r.pdf"), PDF.to_vec());
    let started = std::time::Instant::now();
    let msg = prepare_inbound(&fx.api, "", &files, &fx.opts, &CancellationToken::new())
        .await
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(msg.rejection_notice().unwrap().contains("timed out"));
    assert!(entries(msg.dir().unwrap()).is_empty());
}

#[tokio::test]
async fn download_calls_use_the_preferred_url() {
    let fx = fixture();
    let mut f = file("F1", "a.txt", "text/plain", 1);
    f.url_private_download = None; // falls back to url_private
    fx.api.add_file(&url("F1", "private"), b"x".to_vec());
    let msg = prepare_inbound(&fx.api, "", &[f], &fx.opts, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(msg.text_files.len(), 1);
    assert_eq!(
        fx.api.calls(),
        vec![RecordedCall::DownloadFile {
            url_private: url("F1", "private")
        }]
    );
}

#[test]
fn default_root_lives_under_the_shared_state_dir() {
    let root = default_inbound_root().expect("HOME or XDG_STATE_HOME is set in tests");
    let state = augmentagent_channel_core::state_dir::state_dir().unwrap();
    assert_eq!(root, state.join("slack-inbound"));
    assert!(!root.starts_with("/tmp"));
}

#[tokio::test]
async fn a_symlinked_root_is_refused() {
    let fx = fixture();
    let elsewhere = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(fx.root.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(elsewhere.path(), &fx.root).unwrap();
    let files = vec![file("F1", "a.txt", "text/plain", 1)];
    fx.api.add_file(&url("F1", "a.txt"), b"x".to_vec());
    let err = prepare_inbound(&fx.api, "", &files, &fx.opts, &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(err, InboundError::Storage(_)), "{err:?}");
    assert!(entries(elsewhere.path()).is_empty());
}
