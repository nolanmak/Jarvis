use super::*;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio_tungstenite::{accept_async, tungstenite::Message};

fn started() -> Message {
    Message::Text(
        json!({"type":"session.started","event_id":"evt-start","session":{
        "id":"session-test","model":"gpt-live-1",
        "audio":{"format":{"type":"audio/pcmu","rate":8000},"output":{"voice":"gleam"}},
        "delegation":{"type":"client"}}})
        .to_string()
        .into(),
    )
}

#[tokio::test]
async fn socket_handshake_startup_audio_and_final_usage() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}/", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(tcp).await.unwrap();
        let start: Value =
            serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(start["type"], "session.start");
        assert_eq!(start["session"]["model"], "gpt-live-1");
        ws.send(started()).await.unwrap();
        let audio: Value =
            serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(audio["type"], "session.input_audio.append");
        ws.send(Message::Text(
            json!({"type":"session.output_audio.delta","delta":audio["audio"]})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
        let close: Value =
            serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(close["type"], "session.close");
        // Audio already in flight must drain without being played during close.
        ws.send(Message::Text(
            json!({"type":"session.output_audio.delta","delta":audio["audio"]})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
        ws.send(Message::Text(
            json!({"type":"session.closed","event_id":"evt-close","reason":"close_requested",
            "session":{"id":"session-test"},"usage":{"seconds":2.5}})
            .to_string()
            .into(),
        ))
        .await
        .unwrap();
    });
    let mut connection = LiveConnection::connect_to(&endpoint, "synthetic-key", "Speak briefly.")
        .await
        .unwrap();
    assert_eq!(connection.session_id(), Some("session-test"));
    connection.send_audio(&[255; 160]).await.unwrap();
    assert!(
        matches!(connection.next().await.unwrap(), LiveInput::Audio(bytes) if bytes == vec![255;160])
    );
    assert!(matches!(
        connection.finish().await.unwrap(),
        LiveInput::Closed {
            seconds: Some(2.5),
            ..
        }
    ));
    server.await.unwrap();
}

#[tokio::test]
async fn silent_peer_cannot_hold_startup_forever() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}/", listener.local_addr().unwrap());
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(tcp).await.unwrap();
        let _ = ws.next().await;
        ready_tx.send(()).unwrap();
        std::future::pending::<()>().await;
    });
    let connect = tokio::spawn(async move {
        LiveConnection::connect_to(&endpoint, "synthetic-key", "Speak briefly.").await
    });
    ready_rx.await.unwrap();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(10)).await;
    assert_eq!(
        connect.await.unwrap().err(),
        Some(LiveError::StartupTimeout)
    );
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn lost_socket_does_not_report_success() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}/", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(tcp).await.unwrap();
        let _ = ws.next().await;
        ws.send(started()).await.unwrap();
        ws.close(None).await.unwrap();
    });
    let mut connection = LiveConnection::connect_to(&endpoint, "synthetic-key", "Speak briefly.")
        .await
        .unwrap();
    assert_eq!(
        connection.next().await.err(),
        Some(LiveError::ConnectionLost)
    );
    server.await.unwrap();
}

#[tokio::test]
async fn invalid_header_credentials_are_rejected_locally() {
    for key in ["", "\r\nprivate"] {
        assert_eq!(
            LiveConnection::connect_to("ws://127.0.0.1:1/", key, "Speak briefly.")
                .await
                .err(),
            Some(LiveError::Authentication)
        );
    }
}

#[tokio::test]
async fn handshake_auth_is_sent_and_http_failures_are_redacted() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for (status, expected) in [
        (401, LiveError::Authentication),
        (403, LiveError::Access),
        (429, LiveError::RateLimit),
        (503, LiveError::Unavailable),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("ws://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut tcp, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let byte = tcp.read_u8().await.unwrap();
                request.push(byte);
                assert!(request.len() < 8192);
                if request.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
            assert!(request.contains("authorization: bearer synthetic-key\r\n"));
            let response = format!("HTTP/1.1 {status} Rejected\r\nContent-Length: 19\r\nConnection: close\r\n\r\nprivate secret body");
            tcp.write_all(response.as_bytes()).await.unwrap();
        });
        let error = LiveConnection::connect_to(&endpoint, "synthetic-key", "Speak briefly.")
            .await
            .err()
            .unwrap();
        assert_eq!(error, expected);
        assert!(!error.to_string().contains("secret"));
        server.await.unwrap();
    }
}

#[tokio::test]
async fn finalization_timeout_drops_the_socket() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}/", listener.local_addr().unwrap());
    let (closing_tx, closing_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(tcp).await.unwrap();
        let _ = ws.next().await;
        ws.send(started()).await.unwrap();
        let close = ws.next().await.unwrap().unwrap();
        assert!(close.to_text().unwrap().contains("session.close"));
        closing_tx.send(()).unwrap();
        // No final session.closed event: the client must tear down anyway.
        assert!(matches!(ws.next().await, None | Some(Err(_))));
    });
    let connection = LiveConnection::connect_to(&endpoint, "synthetic-key", "Speak briefly.")
        .await
        .unwrap();
    let finish = tokio::spawn(connection.finish());
    closing_rx.await.unwrap();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(5)).await;
    assert_eq!(finish.await.unwrap().err(), Some(LiveError::CloseTimeout));
    server.await.unwrap();
}

#[tokio::test]
async fn secure_connection_reports_tls_failure_without_panicking() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("wss://localhost:{}/", listener.local_addr().unwrap().port());
    let server = tokio::spawn(async move {
        let (mut tcp, _) = listener.accept().await.unwrap();
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut buf = [0; 4096];
        let _ = tcp.read(&mut buf).await;
        let _ = tcp.write_all(b"invalid TLS").await;
    });
    let result = tokio::spawn(async move {
        LiveConnection::connect_test(&endpoint, "synthetic-key", "silent").await
    }).await;
    server.abort();
    assert!(result.is_ok(), "TLS provider selection panicked");
    assert!(matches!(result.unwrap(), Err(LiveError::ConnectionLost)));
}
