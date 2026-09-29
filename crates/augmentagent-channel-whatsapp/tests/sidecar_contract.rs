//! Wire contract against the compiled Go process with the WhatsApp network
//! deliberately disabled. CI builds the Go binary and sets the env var; locally
//! the test runs against the `setup.sh` output and skips when neither exists.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use augmentagent_channel_whatsapp::api::{WaClient, WaError};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

const SETUP_HINT: &str = "run sidecars/wa-sidecar/setup.sh, or set \
                          AUGMENTAGENT_WA_SIDECAR_TEST_BIN to a built wa-sidecar";

fn runnable(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// The `setup.sh` output, located by walking up to the workspace root.
fn repo_local_binary() -> Option<PathBuf> {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    for _ in 0..6 {
        if dir.join("sidecars/wa-sidecar/main.go").exists() {
            let candidate = dir.join("sidecars/wa-sidecar/wa-sidecar");
            return runnable(&candidate).then_some(candidate);
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

/// `None` when no sidecar binary is available, so the test can skip instead of
/// failing on a missing build artifact (the workspace gate never runs `go build`).
fn sidecar_binary() -> Option<PathBuf> {
    match std::env::var("AUGMENTAGENT_WA_SIDECAR_TEST_BIN") {
        Ok(value) if !value.is_empty() => {
            let path = PathBuf::from(value);
            if runnable(&path) {
                return Some(path);
            }
            // A wrong CI path must be diagnosable, so name it rather than
            // quietly falling back to the repo-local build.
            eprintln!(
                "AUGMENTAGENT_WA_SIDECAR_TEST_BIN points at {} which is not an \
                 executable file",
                path.display()
            );
            None
        }
        _ => repo_local_binary(),
    }
}

struct Sidecar(Child);

impl Drop for Sidecar {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn compiled_go_process_matches_rust_wire_contract() {
    let Some(binary) = sidecar_binary() else {
        eprintln!(
            "skipping compiled_go_process_matches_rust_wire_contract: no sidecar \
             binary ({SETUP_HINT})"
        );
        return;
    };
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
    let (cli_events, _cli_receiver) = mpsc::channel(8);
    let cli_client = WaClient::connect(&socket, cli_events).await.unwrap();
    assert_eq!(cli_client.status().await.unwrap()["paired"], false);
    let (status, concurrent_send) = tokio::join!(
        client.status(),
        client.send_text("1@s.whatsapp.net", "hello")
    );
    let status = status.unwrap();
    assert_eq!(status["paired"], false);
    assert_eq!(status["connected"], false);
    assert!(matches!(concurrent_send, Err(WaError::Sidecar { kind, .. }) if kind == "NotPaired"));
    assert!(matches!(
        client.start_pairing().await.unwrap_err(),
        WaError::Sidecar { kind, .. } if kind == "NotConnected"
    ));
    assert!(matches!(
        client.logout("15551234567:2@s.whatsapp.net").await.unwrap_err(),
        WaError::Sidecar { kind, .. } if kind == "NotPaired"
    ));
    assert!(
        matches!(client.list_chats(10).await.unwrap_err(), WaError::Sidecar { kind, .. } if kind == "NotPaired")
    );
    assert!(matches!(
        client.fetch_chat_history("1@s.whatsapp.net", 10).await.unwrap_err(),
        WaError::Sidecar { kind, .. } if kind == "Unavailable"
    ));
    drop(cli_client);
    assert_eq!(client.status().await.unwrap()["connected"], false);
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
