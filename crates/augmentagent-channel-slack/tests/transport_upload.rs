//! #1294 / #1283 — `HttpSlackWebApi::upload_file` over Slack's external
//! upload flow (`files.getUploadURLExternal` → POST bytes to the returned
//! URL → `files.completeUploadExternal`), against a local mock Slack.
//!
//! Every step and every failure is exercised: step 1 fails, the byte upload
//! fails midway, the completion fails, rate limits, timeout, cancellation,
//! size limit and an insecure upload URL. Tokens are synthetic and must never
//! reach the upload host or an error string.

use std::io::Write as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use augmentagent_channel_slack::transport::token::BotToken;
use augmentagent_channel_slack::transport::web::{
    HttpSlackWebApi, RecordedCall, RecordingSlackWebApi, SlackWebApi, Sleeper, UploadFile,
    UploadLimits, UploadSource, UploadedFile, WebApiConfig, WebApiError,
};
use mockito::Matcher;
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

const BOT: &str = "xoxb-test-000";
const CHANNEL: &str = "C00000001";
const THREAD: &str = "1700000000.000100";
const FILENAME: &str = "Q3 report – ü 日本.txt";

#[derive(Default)]
struct RecordingSleeper {
    slept: Mutex<Vec<Duration>>,
}

#[async_trait]
impl Sleeper for RecordingSleeper {
    async fn sleep(&self, duration: Duration) {
        self.slept.lock().unwrap().push(duration);
    }
}

fn client(base_url: String, sleeper: Arc<RecordingSleeper>) -> HttpSlackWebApi {
    HttpSlackWebApi::new(
        BotToken::new(BOT),
        WebApiConfig {
            base_url,
            request_timeout: Duration::from_secs(5),
            max_attempts: 3,
            max_retry_after: Duration::from_secs(30),
        },
    )
    .unwrap()
    .with_sleeper(sleeper)
}

/// A file under a directory whose name has spaces and non-ASCII characters.
fn temp_file(content: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("generated files ü");
    std::fs::create_dir_all(&sub).unwrap();
    let path = sub.join(FILENAME);
    std::fs::File::create(&path)
        .unwrap()
        .write_all(content)
        .unwrap();
    (dir, path)
}

fn upload_req(source: UploadSource) -> UploadFile {
    UploadFile {
        channel: Some(CHANNEL.into()),
        filename: FILENAME.into(),
        source,
        title: Some("Q3 report".into()),
        thread_ts: Some(THREAD.into()),
        initial_comment: None,
        alt_text: None,
    }
}

async fn step1_ok(
    server: &mut mockito::ServerGuard,
    upload_url: &str,
    length: usize,
) -> mockito::Mock {
    server
        .mock("POST", "/files.getUploadURLExternal")
        .match_header("authorization", format!("Bearer {BOT}").as_str())
        .match_header(
            "content-type",
            Matcher::Regex("application/x-www-form-urlencoded".into()),
        )
        .match_body(Matcher::AllOf(vec![
            Matcher::UrlEncoded("filename".into(), FILENAME.into()),
            Matcher::UrlEncoded("length".into(), length.to_string()),
        ]))
        .with_body(
            json!({"ok": true, "upload_url": upload_url, "file_id": "F00000001"}).to_string(),
        )
        .expect(1)
        .create_async()
        .await
}

async fn complete_ok(server: &mut mockito::ServerGuard) -> mockito::Mock {
    server
        .mock("POST", "/files.completeUploadExternal")
        .match_header("authorization", format!("Bearer {BOT}").as_str())
        .match_header("content-type", Matcher::Regex("application/json".into()))
        .match_body(Matcher::PartialJson(json!({
            "files": [{"id": "F00000001", "title": "Q3 report"}],
            "channel_id": CHANNEL,
            "thread_ts": THREAD,
        })))
        .with_body(
            json!({"ok": true, "files": [{"id": "F00000001", "title": "Q3 report",
                "permalink": "https://example-test.slack.com/files/U00000001/F00000001/q3"}]})
            .to_string(),
        )
        .expect(1)
        .create_async()
        .await
}

/// The failure behind an `UploadIncomplete` wrapper, checking the step.
fn incomplete_at<'a>(err: &'a WebApiError, want_step: &str) -> &'a WebApiError {
    match err {
        WebApiError::UploadIncomplete { step, source } => {
            assert_eq!(*step, want_step, "{err:?}");
            source
        }
        other => panic!("expected UploadIncomplete at {want_step}, got {other:?}"),
    }
}

fn assert_no_token(err: &WebApiError) {
    let text = format!("{err} {err:?}");
    assert!(!text.contains(BOT), "token leaked: {text}");
    assert!(!text.contains("/upload/v1/"), "upload URL leaked: {text}");
}

#[tokio::test]
async fn upload_streams_a_path_with_spaces_and_unicode_into_the_thread() {
    let content: Vec<u8> = (0..70_000u32).map(|i| (i % 251) as u8).collect();
    let (_dir, path) = temp_file(&content);
    let mut server = mockito::Server::new_async().await;
    let upload_url = format!("{}/upload/v1/abc", server.url());
    let step1 = step1_ok(&mut server, &upload_url, content.len()).await;
    let upload = server
        .mock("POST", "/upload/v1/abc")
        // The pre-signed upload URL never receives the bot token.
        .match_header("authorization", Matcher::Missing)
        .match_header("content-type", "application/octet-stream")
        .match_header("content-length", content.len().to_string().as_str())
        .match_body(content.clone())
        .with_body("OK - 70000")
        .expect(1)
        .create_async()
        .await;
    let complete = complete_ok(&mut server).await;

    let api = client(server.url(), Arc::new(RecordingSleeper::default()));
    let uploaded = api
        .upload_file(upload_req(UploadSource::Path(path)))
        .await
        .expect("upload");
    assert_eq!(
        uploaded,
        UploadedFile {
            id: "F00000001".into(),
            permalink: Some("https://example-test.slack.com/files/U00000001/F00000001/q3".into()),
        }
    );
    step1.assert_async().await;
    upload.assert_async().await;
    complete.assert_async().await;
}

#[tokio::test]
async fn upload_from_memory_with_comment_and_alt_text() {
    let mut server = mockito::Server::new_async().await;
    let upload_url = format!("{}/upload/v1/mem", server.url());
    let step1 = server
        .mock("POST", "/files.getUploadURLExternal")
        .match_body(Matcher::AllOf(vec![
            Matcher::UrlEncoded("filename".into(), "chart.png".into()),
            Matcher::UrlEncoded("length".into(), "4".into()),
            Matcher::UrlEncoded("alt_txt".into(), "a chart".into()),
        ]))
        .with_body(
            json!({"ok": true, "upload_url": upload_url, "file_id": "F00000002"}).to_string(),
        )
        .create_async()
        .await;
    let upload = server
        .mock("POST", "/upload/v1/mem")
        .match_body(vec![1u8, 2, 3, 4])
        .with_body("OK - 4")
        .create_async()
        .await;
    let complete = server
        .mock("POST", "/files.completeUploadExternal")
        .match_body(Matcher::PartialJson(json!({
            "files": [{"id": "F00000002"}],
            "channel_id": CHANNEL,
            "initial_comment": "here it is",
        })))
        .with_body(json!({"ok": true, "files": [{"id": "F00000002"}]}).to_string())
        .create_async()
        .await;
    let api = client(server.url(), Arc::new(RecordingSleeper::default()));
    let uploaded = api
        .upload_file(UploadFile {
            channel: Some(CHANNEL.into()),
            filename: "chart.png".into(),
            source: UploadSource::Bytes(vec![1, 2, 3, 4]),
            title: None,
            thread_ts: None,
            initial_comment: Some("here it is".into()),
            alt_text: Some("a chart".into()),
        })
        .await
        .expect("upload");
    assert_eq!(uploaded.id, "F00000002");
    assert_eq!(uploaded.permalink, None);
    step1.assert_async().await;
    upload.assert_async().await;
    complete.assert_async().await;
}

#[tokio::test]
async fn step1_failure_sends_no_bytes_and_never_completes() {
    let (_dir, path) = temp_file(b"hello");
    let mut server = mockito::Server::new_async().await;
    server
        .mock("POST", "/files.getUploadURLExternal")
        .with_body(json!({"ok": false, "error": "file_uploads_disabled"}).to_string())
        .expect(1)
        .create_async()
        .await;
    let upload = server
        .mock("POST", "/upload/v1/abc")
        .expect(0)
        .create_async()
        .await;
    let complete = server
        .mock("POST", "/files.completeUploadExternal")
        .expect(0)
        .create_async()
        .await;
    let api = client(server.url(), Arc::new(RecordingSleeper::default()));
    let err = api
        .upload_file(upload_req(UploadSource::Path(path)))
        .await
        .unwrap_err();
    assert!(
        matches!(incomplete_at(&err, "files.getUploadURLExternal"),
            WebApiError::Slack { error, .. } if error == "file_uploads_disabled"),
        "{err:?}"
    );
    assert_no_token(&err);
    upload.assert_async().await;
    complete.assert_async().await;
}

/// Accepts one connection, reads `take` bytes of the request, then drops it.
async fn cut_off_server(take: usize) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/upload/v1/abc", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = vec![0u8; take];
            let _ = sock.read_exact(&mut buf).await;
            drop(sock);
        }
    });
    (url, task)
}

#[tokio::test]
async fn upload_cut_off_midway_is_an_error_and_never_completes() {
    let content = vec![7u8; 4 * 1024 * 1024];
    let (_dir, path) = temp_file(&content);
    let (upload_url, task) = cut_off_server(64 * 1024).await;
    let mut server = mockito::Server::new_async().await;
    let step1 = step1_ok(&mut server, &upload_url, content.len()).await;
    let complete = server
        .mock("POST", "/files.completeUploadExternal")
        .expect(0)
        .create_async()
        .await;
    let api = client(server.url(), Arc::new(RecordingSleeper::default()));
    let err = api
        .upload_file(upload_req(UploadSource::Path(path)))
        .await
        .unwrap_err();
    assert!(
        matches!(
            incomplete_at(&err, "upload"),
            WebApiError::Transport(_) | WebApiError::Http { .. }
        ),
        "{err:?}"
    );
    assert_no_token(&err);
    step1.assert_async().await;
    complete.assert_async().await;
    task.abort();
}

#[tokio::test]
async fn upload_host_error_status_never_completes() {
    let (_dir, path) = temp_file(b"hello");
    let mut server = mockito::Server::new_async().await;
    let upload_url = format!("{}/upload/v1/abc", server.url());
    step1_ok(&mut server, &upload_url, 5).await;
    server
        .mock("POST", "/upload/v1/abc")
        .with_status(500)
        .with_body("upstream exploded")
        .create_async()
        .await;
    let complete = server
        .mock("POST", "/files.completeUploadExternal")
        .expect(0)
        .create_async()
        .await;
    let api = client(server.url(), Arc::new(RecordingSleeper::default()));
    let err = api
        .upload_file(upload_req(UploadSource::Path(path)))
        .await
        .unwrap_err();
    assert!(
        matches!(
            incomplete_at(&err, "upload"),
            WebApiError::Http { status: 500, .. }
        ),
        "{err:?}"
    );
    assert_no_token(&err);
    complete.assert_async().await;
}

#[tokio::test]
async fn complete_failure_is_reported_after_the_bytes_went_up() {
    let (_dir, path) = temp_file(b"hello");
    let mut server = mockito::Server::new_async().await;
    let upload_url = format!("{}/upload/v1/abc", server.url());
    step1_ok(&mut server, &upload_url, 5).await;
    let upload = server
        .mock("POST", "/upload/v1/abc")
        .with_body("OK - 5")
        .expect(1)
        .create_async()
        .await;
    server
        .mock("POST", "/files.completeUploadExternal")
        .with_body(json!({"ok": false, "error": "channel_not_found"}).to_string())
        .expect(1)
        .create_async()
        .await;
    let api = client(server.url(), Arc::new(RecordingSleeper::default()));
    let err = api
        .upload_file(upload_req(UploadSource::Path(path)))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, WebApiError::Slack { error, .. } if error == "channel_not_found"),
        "{err:?}"
    );
    assert_no_token(&err);
    upload.assert_async().await;
}

#[tokio::test]
async fn size_limit_empty_and_missing_files_are_refused_before_any_request() {
    let mut server = mockito::Server::new_async().await;
    let any = server
        .mock("POST", Matcher::Any)
        .expect(0)
        .create_async()
        .await;
    let api = client(server.url(), Arc::new(RecordingSleeper::default())).with_upload_limits(
        UploadLimits {
            max_bytes: 8,
            ..UploadLimits::default()
        },
    );

    let (_dir, big) = temp_file(b"123456789");
    let err = api
        .upload_file(upload_req(UploadSource::Path(big)))
        .await
        .unwrap_err();
    assert!(
        matches!(err, WebApiError::FileTooLarge { size: 9, limit: 8 }),
        "{err:?}"
    );

    let (_dir2, empty) = temp_file(b"");
    let err = api
        .upload_file(upload_req(UploadSource::Path(empty)))
        .await
        .unwrap_err();
    assert!(matches!(err, WebApiError::InvalidUpload(_)), "{err:?}");

    let err = api
        .upload_file(upload_req(UploadSource::Path(
            "/nonexistent/dir ü/x.txt".into(),
        )))
        .await
        .unwrap_err();
    assert!(matches!(err, WebApiError::InvalidUpload(_)), "{err:?}");

    let mut req = upload_req(UploadSource::Bytes(b"x".to_vec()));
    req.filename = "  ".into();
    let err = api.upload_file(req).await.unwrap_err();
    assert!(matches!(err, WebApiError::InvalidUpload(_)), "{err:?}");
    any.assert_async().await;
    assert!(UploadLimits::default().max_bytes >= 1024 * 1024 * 1024);
}

/// A loopback listener that accepts and never answers.
async fn silent_server() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/upload/v1/abc", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((sock, _)) = listener.accept().await {
            held.push(sock);
        }
    });
    (url, task)
}

#[tokio::test]
async fn stalled_upload_times_out_and_cancel_releases_it() {
    let (upload_url, task) = silent_server().await;
    let mut server = mockito::Server::new_async().await;
    server
        .mock("POST", "/files.getUploadURLExternal")
        .with_body(
            json!({"ok": true, "upload_url": upload_url, "file_id": "F00000001"}).to_string(),
        )
        .create_async()
        .await;
    let complete = server
        .mock("POST", "/files.completeUploadExternal")
        .expect(0)
        .create_async()
        .await;

    let api = client(server.url(), Arc::new(RecordingSleeper::default())).with_upload_limits(
        UploadLimits {
            transfer_timeout: Duration::from_millis(100),
            ..UploadLimits::default()
        },
    );
    let started = std::time::Instant::now();
    let err = tokio::time::timeout(
        Duration::from_secs(5),
        api.upload_file(upload_req(UploadSource::Bytes(b"hello".to_vec()))),
    )
    .await
    .expect("waiter released")
    .unwrap_err();
    assert!(
        matches!(incomplete_at(&err, "upload"), WebApiError::Timeout),
        "{err:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(3));

    let cancel = CancellationToken::new();
    let scoped = client(server.url(), Arc::new(RecordingSleeper::default())).scoped(cancel.clone());
    let call = tokio::spawn(async move {
        scoped
            .upload_file(upload_req(UploadSource::Bytes(b"hello".to_vec())))
            .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    cancel.cancel();
    let err = tokio::time::timeout(Duration::from_secs(5), call)
        .await
        .expect("waiter released")
        .unwrap()
        .unwrap_err();
    assert!(matches!(err.root(), WebApiError::Cancelled), "{err:?}");
    complete.assert_async().await;
    task.abort();
}

#[tokio::test]
async fn rate_limits_step1_is_retried_and_a_limited_upload_is_surfaced() {
    let mut server = mockito::Server::new_async().await;
    let upload_url = format!("{}/upload/v1/abc", server.url());
    let limited = server
        .mock("POST", "/files.getUploadURLExternal")
        .with_status(429)
        .with_header("retry-after", "2")
        .expect(1)
        .create_async()
        .await;
    let ok = server
        .mock("POST", "/files.getUploadURLExternal")
        .with_body(
            json!({"ok": true, "upload_url": upload_url, "file_id": "F00000001"}).to_string(),
        )
        .expect(1)
        .create_async()
        .await;
    server
        .mock("POST", "/upload/v1/abc")
        .with_status(429)
        .with_header("retry-after", "9")
        .create_async()
        .await;
    let complete = server
        .mock("POST", "/files.completeUploadExternal")
        .expect(0)
        .create_async()
        .await;
    let sleeper = Arc::new(RecordingSleeper::default());
    let api = client(server.url(), sleeper.clone());
    let err = api
        .upload_file(upload_req(UploadSource::Bytes(b"hello".to_vec())))
        .await
        .unwrap_err();
    assert!(
        matches!(incomplete_at(&err, "upload"),
            WebApiError::RateLimited { retry_after } if *retry_after == Duration::from_secs(9)),
        "{err:?}"
    );
    assert_eq!(
        sleeper.slept.lock().unwrap().as_slice(),
        &[Duration::from_secs(2)]
    );
    limited.assert_async().await;
    ok.assert_async().await;
    complete.assert_async().await;
}

#[tokio::test]
async fn insecure_or_foreign_upload_url_is_refused_without_echoing_it() {
    for bad in [
        "http://files.example.test/upload/v1/abc",
        "ftp://127.0.0.1/upload/v1/abc",
        "https://user:pw@files.example.test/upload/v1/abc",
        "not a url /upload/v1/abc",
    ] {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/files.getUploadURLExternal")
            .with_body(json!({"ok": true, "upload_url": bad, "file_id": "F00000001"}).to_string())
            .create_async()
            .await;
        let complete = server
            .mock("POST", "/files.completeUploadExternal")
            .expect(0)
            .create_async()
            .await;
        let api = client(server.url(), Arc::new(RecordingSleeper::default()));
        let err = api
            .upload_file(upload_req(UploadSource::Bytes(b"hello".to_vec())))
            .await
            .unwrap_err();
        assert!(
            matches!(err, WebApiError::InvalidUpload(_)),
            "{bad}: {err:?}"
        );
        assert_no_token(&err);
        complete.assert_async().await;
    }
}

#[tokio::test]
async fn recording_fake_records_uploads_from_paths() {
    let (_dir, path) = temp_file(b"12345");
    let fake = RecordingSlackWebApi::default();
    let up = fake
        .upload_file(upload_req(UploadSource::Path(path)))
        .await
        .unwrap();
    assert!(up.id.starts_with('F'));
    assert_eq!(
        fake.calls(),
        vec![RecordedCall::UploadFile {
            filename: FILENAME.into(),
            channel: Some(CHANNEL.into()),
            thread_ts: Some(THREAD.into()),
            bytes: 5,
        }]
    );
}
