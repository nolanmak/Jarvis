//! #1293 / #1283 — `HttpSlackWebApi::download_file`: an authenticated GET of
//! a Slack `url_private(_download)` streamed into a private file, against a
//! local mock file host.
//!
//! Covered: bearer token only to allow-listed hosts, redirects (same host
//! keeps the token, another allow-listed host gets none, a foreign host is
//! refused before any request), size cap from `Content-Length` and while
//! streaming, a Slack sign-in page instead of the file, 429 with
//! `Retry-After`, HTTP errors, a stalled transfer timing out, cancellation,
//! and that no partial file or token is ever left behind. Synthetic tokens
//! only; the "foreign" host is another loopback port.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use augmentagent_channel_slack::transport::token::BotToken;
use augmentagent_channel_slack::transport::web::{
    file_host_allowed, test_file_hosts_from, DownloadLimits, DownloadRequest, HttpSlackWebApi,
    RecordedCall, RecordingSlackWebApi, SlackWebApi, Sleeper, WebApiConfig, WebApiError,
    SLACK_FILE_HOSTS,
};
use mockito::Matcher;
use tokio_util::sync::CancellationToken;

const BOT: &str = "xoxb-test-000";

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

fn host_of(url: &str) -> String {
    url.trim_start_matches("http://").to_string()
}

fn client(hosts: &[&str], sleeper: Arc<RecordingSleeper>, timeout: Duration) -> HttpSlackWebApi {
    HttpSlackWebApi::new(
        BotToken::new(BOT),
        WebApiConfig {
            base_url: "http://127.0.0.1:9/api".into(),
            request_timeout: Duration::from_secs(5),
            max_attempts: 3,
            max_retry_after: Duration::from_secs(30),
        },
    )
    .unwrap()
    .with_sleeper(sleeper)
    .with_download_limits(DownloadLimits {
        allowed_hosts: hosts.iter().map(|h| h.to_string()).collect(),
        transfer_timeout: timeout,
        max_redirects: 3,
    })
}

/// Destination inside a private dir whose path has spaces and Unicode.
fn new_dest() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("inbound ü 日本");
    std::fs::create_dir(&sub).unwrap();
    (dir, sub.join("00-report.txt"))
}

fn req<'a>(url: &'a str, dest: &'a Path, max: u64) -> DownloadRequest<'a> {
    DownloadRequest {
        url,
        dest,
        max_bytes: max,
        expected_mimetype: Some("text/plain"),
    }
}

fn assert_clean(err: &WebApiError, dest: &Path) {
    let text = format!("{err} {err:?}");
    assert!(!text.contains(BOT), "token leaked: {text}");
    assert!(!dest.exists(), "partial file left at {}", dest.display());
}

#[test]
fn only_documented_slack_file_hosts_are_allowed_by_default() {
    assert_eq!(SLACK_FILE_HOSTS, &["files.slack.com"]);
    let hosts: Vec<String> = SLACK_FILE_HOSTS.iter().map(|h| h.to_string()).collect();
    for ok in [
        "https://files.slack.com/files-pri/T00000001-F00000001/report.txt",
        "https://FILES.slack.com/files-pri/T00000001-F00000001/download/report.txt",
    ] {
        assert!(file_host_allowed(ok, &hosts).is_ok(), "{ok}");
    }
    for bad in [
        "http://files.slack.com/files-pri/x",    // not https
        "https://files.slack.com.example.com/x", // suffix trick
        "https://evil-files.slack.com/x",        // other subdomain
        "https://example.com/files.slack.com/x", // path trick
        "https://user:pw@files.slack.com/x",     // credentials in URL (synthetic; pii-ok)
        "https://files.slack.com:8443/x",        // other port
        "ftp://files.slack.com/x",
        "not a url",
    ] {
        assert!(file_host_allowed(bad, &hosts).is_err(), "{bad}");
    }
}

#[test]
fn test_file_host_override_accepts_loopback_only() {
    assert_eq!(
        test_file_hosts_from(Some("127.0.0.1:8080, localhost:9")).unwrap(),
        vec!["127.0.0.1:8080".to_string(), "localhost:9".to_string()]
    );
    assert!(test_file_hosts_from(None).unwrap().is_empty());
    assert!(test_file_hosts_from(Some("  ")).unwrap().is_empty());
    for bad in [
        "files.slack.com",
        "10.0.0.1:80",
        "example.com:80",
        "127.0.0.1",
    ] {
        assert!(test_file_hosts_from(Some(bad)).is_err(), "{bad}");
    }
    // Loopback http is allowed only for a listed host:port.
    let hosts = vec!["127.0.0.1:8080".to_string()];
    assert!(file_host_allowed("http://127.0.0.1:8080/f", &hosts).is_ok());
    assert!(file_host_allowed("http://127.0.0.1:8081/f", &hosts).is_err());
}

#[tokio::test]
async fn download_streams_with_the_bot_token_into_a_private_file() {
    let body: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let mut server = mockito::Server::new_async().await;
    let file = server
        .mock("GET", "/files-pri/T00000001-F00000001/download/report.txt")
        .match_header("authorization", format!("Bearer {BOT}").as_str())
        .with_header("content-type", "text/plain")
        .with_body(body.clone())
        .expect(1)
        .create_async()
        .await;
    let host = host_of(&server.url());
    let api = client(&[&host], Arc::default(), Duration::from_secs(5));
    let (_dir, dest) = new_dest();
    let url = format!(
        "{}/files-pri/T00000001-F00000001/download/report.txt",
        server.url()
    );
    let n = api
        .download_file(req(&url, &dest, 1_000_000))
        .await
        .unwrap();
    assert_eq!(n, body.len() as u64);
    assert_eq!(std::fs::read(&dest).unwrap(), body);
    assert_eq!(
        std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777,
        0o600
    );
    file.assert_async().await;
}

#[tokio::test]
async fn a_host_off_the_allow_list_is_refused_before_any_request() {
    let mut foreign = mockito::Server::new_async().await;
    let never = foreign
        .mock("GET", Matcher::Any)
        .expect(0)
        .create_async()
        .await;
    let api = client(&["127.0.0.1:1"], Arc::default(), Duration::from_secs(5));
    let (_dir, dest) = new_dest();
    let url = format!("{}/files-pri/x", foreign.url());
    let err = api.download_file(req(&url, &dest, 100)).await.unwrap_err();
    assert!(matches!(err, WebApiError::FileHostRefused(_)), "{err:?}");
    assert_clean(&err, &dest);
    never.assert_async().await;
}

#[tokio::test]
async fn same_host_redirect_keeps_the_token_and_is_followed() {
    let mut server = mockito::Server::new_async().await;
    let first = server
        .mock("GET", "/files-pri/a")
        .match_header("authorization", format!("Bearer {BOT}").as_str())
        .with_status(302)
        .with_header("location", "/files-pri/a/real")
        .expect(1)
        .create_async()
        .await;
    let second = server
        .mock("GET", "/files-pri/a/real")
        .match_header("authorization", format!("Bearer {BOT}").as_str())
        .with_body("hello")
        .expect(1)
        .create_async()
        .await;
    let host = host_of(&server.url());
    let api = client(&[&host], Arc::default(), Duration::from_secs(5));
    let (_dir, dest) = new_dest();
    let url = format!("{}/files-pri/a", server.url());
    assert_eq!(api.download_file(req(&url, &dest, 100)).await.unwrap(), 5);
    first.assert_async().await;
    second.assert_async().await;
}

#[tokio::test]
async fn redirect_to_another_allowed_host_drops_the_token() {
    let mut slack = mockito::Server::new_async().await;
    let mut cdn = mockito::Server::new_async().await;
    let hop = slack
        .mock("GET", "/files-pri/b")
        .match_header("authorization", format!("Bearer {BOT}").as_str())
        .with_status(302)
        .with_header("location", &format!("{}/signed/b?sig=1", cdn.url()))
        .expect(1)
        .create_async()
        .await;
    let fetched = cdn
        .mock("GET", "/signed/b?sig=1")
        .match_header("authorization", Matcher::Missing)
        .with_body("from cdn")
        .expect(1)
        .create_async()
        .await;
    let (h1, h2) = (host_of(&slack.url()), host_of(&cdn.url()));
    let api = client(&[&h1, &h2], Arc::default(), Duration::from_secs(5));
    let (_dir, dest) = new_dest();
    let url = format!("{}/files-pri/b", slack.url());
    assert_eq!(api.download_file(req(&url, &dest, 100)).await.unwrap(), 8);
    hop.assert_async().await;
    fetched.assert_async().await;
}

#[tokio::test]
async fn redirect_to_a_foreign_host_is_refused_and_the_token_never_sent() {
    let mut slack = mockito::Server::new_async().await;
    let mut foreign = mockito::Server::new_async().await;
    let hop = slack
        .mock("GET", "/files-pri/c")
        .with_status(302)
        .with_header("location", &format!("{}/steal", foreign.url()))
        .expect(1)
        .create_async()
        .await;
    let never = foreign
        .mock("GET", Matcher::Any)
        .expect(0)
        .create_async()
        .await;
    let host = host_of(&slack.url());
    let api = client(&[&host], Arc::default(), Duration::from_secs(5));
    let (_dir, dest) = new_dest();
    let url = format!("{}/files-pri/c", slack.url());
    let err = api.download_file(req(&url, &dest, 100)).await.unwrap_err();
    assert!(matches!(err, WebApiError::FileHostRefused(_)), "{err:?}");
    assert!(err.to_string().contains("redirect"), "{err}");
    assert_clean(&err, &dest);
    hop.assert_async().await;
    never.assert_async().await;
}

#[tokio::test]
async fn redirect_loops_stop_at_the_limit() {
    let mut server = mockito::Server::new_async().await;
    let _loop = server
        .mock("GET", "/files-pri/loop")
        .with_status(302)
        .with_header("location", "/files-pri/loop")
        .create_async()
        .await;
    let host = host_of(&server.url());
    let api = client(&[&host], Arc::default(), Duration::from_secs(5));
    let (_dir, dest) = new_dest();
    let url = format!("{}/files-pri/loop", server.url());
    let err = api.download_file(req(&url, &dest, 100)).await.unwrap_err();
    assert!(matches!(err, WebApiError::FileHostRefused(_)), "{err:?}");
    assert_clean(&err, &dest);
}

#[tokio::test]
async fn declared_length_over_the_cap_is_refused_before_the_body() {
    let mut server = mockito::Server::new_async().await;
    let _m = server
        .mock("GET", "/files-pri/big")
        .with_body(vec![b'x'; 2048])
        .create_async()
        .await;
    let host = host_of(&server.url());
    let api = client(&[&host], Arc::default(), Duration::from_secs(5));
    let (_dir, dest) = new_dest();
    let url = format!("{}/files-pri/big", server.url());
    let err = api.download_file(req(&url, &dest, 1024)).await.unwrap_err();
    assert!(
        matches!(
            err,
            WebApiError::FileTooLarge {
                size: 2048,
                limit: 1024
            }
        ),
        "{err:?}"
    );
    assert_clean(&err, &dest);
}

#[tokio::test]
async fn the_cap_is_enforced_while_streaming_without_a_length() {
    let mut server = mockito::Server::new_async().await;
    let _m = server
        .mock("GET", "/files-pri/chunked")
        .with_chunked_body(|w| {
            for _ in 0..64 {
                w.write_all(&[b'y'; 1024])?;
            }
            Ok(())
        })
        .create_async()
        .await;
    let host = host_of(&server.url());
    let api = client(&[&host], Arc::default(), Duration::from_secs(5));
    let (_dir, dest) = new_dest();
    let url = format!("{}/files-pri/chunked", server.url());
    let err = api
        .download_file(req(&url, &dest, 10_000))
        .await
        .unwrap_err();
    match &err {
        WebApiError::FileTooLarge { size, limit } => {
            assert_eq!(*limit, 10_000);
            assert!(*size > 10_000 && *size < 64 * 1024, "stopped late: {size}");
        }
        other => panic!("expected FileTooLarge, got {other:?}"),
    }
    assert_clean(&err, &dest);
}

#[tokio::test]
async fn a_sign_in_page_instead_of_the_file_is_rejected() {
    let mut server = mockito::Server::new_async().await;
    let _m = server
        .mock("GET", "/files-pri/login")
        .with_header("content-type", "text/html; charset=utf-8")
        .with_body("<html>sign in</html>")
        .create_async()
        .await;
    let host = host_of(&server.url());
    let api = client(&[&host], Arc::default(), Duration::from_secs(5));
    let (_dir, dest) = new_dest();
    let url = format!("{}/files-pri/login", server.url());
    let err = api.download_file(req(&url, &dest, 1000)).await.unwrap_err();
    assert!(matches!(err, WebApiError::DownloadRejected(_)), "{err:?}");
    assert!(err.to_string().contains("files:read"), "{err}");
    assert_clean(&err, &dest);
    // An HTML file the owner actually sent is kept.
    let (_dir2, dest2) = new_dest();
    let ok = DownloadRequest {
        expected_mimetype: Some("text/html"),
        ..req(&url, &dest2, 1000)
    };
    assert_eq!(api.download_file(ok).await.unwrap(), 20);
}

/// A loopback host that answers successive connections with `responses`,
/// in order, and records whether each request carried the bot token.
async fn scripted_host(
    responses: Vec<&'static str>,
) -> (String, Arc<Mutex<Vec<bool>>>, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = seen.clone();
    let task = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for response in responses {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let head = String::from_utf8_lossy(&buf[..n]).to_lowercase();
            record
                .lock()
                .unwrap()
                .push(head.contains("authorization: bearer"));
            let _ = sock.write_all(response.as_bytes()).await;
            let _ = sock.shutdown().await;
        }
    });
    (format!("127.0.0.1:{}", addr.port()), seen, task)
}

#[tokio::test]
async fn rate_limit_waits_for_retry_after_then_succeeds() {
    let (host, seen, task) = scripted_host(vec![
        "HTTP/1.1 429 Too Many Requests\r\nretry-after: 2\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
        "HTTP/1.1 200 OK\r\ncontent-length: 5\r\nconnection: close\r\n\r\nafter",
    ])
    .await;
    let sleeper = Arc::new(RecordingSleeper::default());
    let api = client(&[&host], sleeper.clone(), Duration::from_secs(5));
    let (_dir, dest) = new_dest();
    let url = format!("http://{host}/files-pri/r");
    assert_eq!(api.download_file(req(&url, &dest, 100)).await.unwrap(), 5);
    assert_eq!(std::fs::read(&dest).unwrap(), b"after");
    assert_eq!(*sleeper.slept.lock().unwrap(), vec![Duration::from_secs(2)]);
    assert_eq!(*seen.lock().unwrap(), vec![true, true]);
    task.abort();
}

#[tokio::test]
async fn rate_limit_past_the_cap_is_returned_and_http_errors_are_redacted() {
    let mut server = mockito::Server::new_async().await;
    let _m = server
        .mock("GET", "/files-pri/slow")
        .with_status(429)
        .with_header("retry-after", "3600")
        .create_async()
        .await;
    let _n = server
        .mock("GET", "/files-pri/missing")
        .with_status(404)
        .with_body(format!("no such file for {BOT}"))
        .create_async()
        .await;
    let host = host_of(&server.url());
    let api = client(&[&host], Arc::default(), Duration::from_secs(5));
    let (_dir, dest) = new_dest();
    let url = format!("{}/files-pri/slow", server.url());
    let err = api.download_file(req(&url, &dest, 100)).await.unwrap_err();
    assert!(matches!(err, WebApiError::RateLimited { .. }), "{err:?}");
    assert_clean(&err, &dest);
    let url = format!("{}/files-pri/missing", server.url());
    let err = api.download_file(req(&url, &dest, 100)).await.unwrap_err();
    assert!(
        matches!(err, WebApiError::Http { status: 404, .. }),
        "{err:?}"
    );
    assert_clean(&err, &dest);
}

/// A file host that sends headers and part of the body, then stalls.
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
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 100000\r\n\r\npartial")
                    .await;
                tokio::time::sleep(Duration::from_secs(30)).await;
            });
        }
    });
    (format!("127.0.0.1:{}", addr.port()), task)
}

#[tokio::test]
async fn a_stalled_transfer_times_out_and_leaves_no_partial_file() {
    let (host, task) = stalling_host().await;
    let api = client(&[&host], Arc::default(), Duration::from_millis(400));
    let (_dir, dest) = new_dest();
    let url = format!("http://{host}/files-pri/stall");
    let err = api
        .download_file(req(&url, &dest, 200_000))
        .await
        .unwrap_err();
    assert!(matches!(err, WebApiError::Timeout), "{err:?}");
    assert_clean(&err, &dest);
    task.abort();
}

#[tokio::test]
async fn cancellation_stops_the_transfer_and_leaves_no_partial_file() {
    let (host, task) = stalling_host().await;
    let cancel = CancellationToken::new();
    let api = client(&[&host], Arc::default(), Duration::from_secs(30)).scoped(cancel.clone());
    let (_dir, dest) = new_dest();
    let url = format!("http://{host}/files-pri/stall");
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        trigger.cancel();
    });
    let err = api
        .download_file(req(&url, &dest, 200_000))
        .await
        .unwrap_err();
    assert!(matches!(err, WebApiError::Cancelled), "{err:?}");
    assert_clean(&err, &dest);
    task.abort();
}

#[tokio::test]
async fn an_existing_destination_is_never_overwritten() {
    let api = client(&["127.0.0.1:1"], Arc::default(), Duration::from_secs(5));
    let (_dir, dest) = new_dest();
    std::fs::write(&dest, b"keep me").unwrap();
    let err = api
        .download_file(req("http://127.0.0.1:1/files-pri/x", &dest, 100))
        .await
        .unwrap_err();
    assert!(matches!(err, WebApiError::InvalidRequest(_)), "{err:?}");
    assert_eq!(std::fs::read(&dest).unwrap(), b"keep me");
}

#[tokio::test]
async fn recording_fake_serves_scripted_files_within_the_cap() {
    let fake = RecordingSlackWebApi::default();
    fake.add_file(
        "https://files.slack.com/files-pri/T1-F1/a.txt",
        b"abc".to_vec(),
    );
    let (_dir, dest) = new_dest();
    let url = "https://files.slack.com/files-pri/T1-F1/a.txt";
    assert_eq!(fake.download_file(req(url, &dest, 10)).await.unwrap(), 3);
    assert_eq!(std::fs::read(&dest).unwrap(), b"abc");
    let (_d2, dest2) = new_dest();
    let err = fake.download_file(req(url, &dest2, 2)).await.unwrap_err();
    assert!(matches!(err, WebApiError::FileTooLarge { .. }), "{err:?}");
    assert!(!dest2.exists());
    assert_eq!(
        fake.calls()[0],
        RecordedCall::DownloadFile {
            url_private: url.into()
        }
    );
}
