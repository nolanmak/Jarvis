//! Wire contract against the compiled Go process with the WhatsApp network
//! deliberately disabled. CI builds the Go binary and sets the env var.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use augmentagent_channel_whatsapp::api::{WaClient, WaError};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

struct Sidecar(Child);

impl Drop for Sidecar {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn compiled_go_process_matches_rust_wire_contract() {
    let binary = std::env::var("AUGMENTAGENT_WA_SIDECAR_TEST_BIN")
        .expect("build the Go sidecar and set AUGMENTAGENT_WA_SIDECAR_TEST_BIN");
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("wa.sock");
    let _child = Sidecar(
        Command::new(binary)
            .arg("--offline-test")
            .env("AUGMENTAGENT_WA_SOCK", &socket)
            .env("AUGMENTAGENT_WA_STORE", temp.path().join("store.db"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(socket.exists(), "sidecar did not create its socket");
    let (events, _receiver) = mpsc::channel(8);
    let client = WaClient::connect(&socket, events).await.unwrap();
    let (status, concurrent_send) = tokio::join!(
        client.status(),
        client.send_text("1@s.whatsapp.net", "hello")
    );
    let status = status.unwrap();
    assert_eq!(status["paired"], false);
    assert_eq!(status["connected"], false);
    assert!(matches!(concurrent_send, Err(WaError::Sidecar { kind, .. }) if kind == "NotPaired"));
    assert!(
        matches!(client.list_chats(10).await.unwrap_err(), WaError::Sidecar { kind, .. } if kind == "NotPaired")
    );
    assert!(matches!(
        client.fetch_chat_history("1@s.whatsapp.net", 10).await.unwrap_err(),
        WaError::Sidecar { kind, .. } if kind == "Unavailable"
    ));
    drop(client);

    let stream = UnixStream::connect(&socket).await.unwrap();
    let (read, mut write) = stream.into_split();
    write
        .write_all(b"{\"version\":999,\"request_id\":\"bad\",\"op\":\"status\",\"params\":{}}\n")
        .await
        .unwrap();
    let line = tokio::time::timeout(
        Duration::from_secs(2),
        BufReader::new(read).lines().next_line(),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap();
    let response: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(response["request_id"], "bad");
    assert_eq!(response["error"]["kind"], "BadRequest");
}
