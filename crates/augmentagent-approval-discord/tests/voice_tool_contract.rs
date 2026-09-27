use augmentagent_approval_discord::voice_bridge::{VoiceBinding, VoiceBridge};
use augmentagent_approval_discord::voice_tool::VoiceToolService;
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::test]
async fn tool_grant_binds_speech_to_one_active_conversation_and_rejects_spoofing() {
    let directory = tempfile::tempdir().unwrap();
    let sidecar_path = directory.path().join("sidecar.sock");
    let listener = tokio::net::UnixListener::bind(&sidecar_path).unwrap();
    let sidecar = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        for expected in ["start", "speak", "status", "stop"] {
            let frame: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(frame["kind"], expected);
            assert_eq!(frame["conversationId"], "1:2");
            let mut reply =
                json!({"version":1,"kind":"reply","requestId":frame["requestId"],"ok":true});
            if expected == "speak" {
                assert_eq!(frame["text"], "Hello from the agent");
                reply["receipt"] = json!({"utteranceId":frame["utteranceId"],"status":"queued"});
            }
            if expected == "status" {
                reply["state"] = json!("listening");
            }
            write
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .unwrap();
        }
    });
    let bridge = VoiceBridge::connect(&sidecar_path).await.unwrap();
    bridge
        .start(VoiceBinding {
            guild_id: "1".into(),
            conversation_id: "1:2".into(),
            text_channel_id: "2".into(),
            voice_channel_id: "3".into(),
            owner_id: "4".into(),
            bot_user_id: "5".into(),
            generation: 6,
        })
        .await
        .unwrap();
    let service = VoiceToolService::start(&bridge).await.unwrap();
    assert_eq!(
        std::fs::metadata(service.socket_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert!(service.grant("1", "1:9", "turn-1").is_none());
    let grant = service.grant("1", "1:2", "turn-1").unwrap();
    let token = grant.token().to_string();
    let spoof = call(
        service.socket_path(),
        "wrong-token",
        "voice_status",
        json!({}),
    )
    .await;
    assert_eq!(spoof["ok"], false);
    let spoke = call(
        service.socket_path(),
        grant.token(),
        "speak",
        json!({"text":"Hello from the agent","utterance_id":"final"}),
    )
    .await;
    assert_eq!(spoke["ok"], true);
    assert_eq!(spoke["receipt"]["status"], "queued");
    assert_eq!(
        spoke["mirrored"], false,
        "missing text transport is reported on the receipt"
    );
    let duplicate = call(
        service.socket_path(),
        grant.token(),
        "speak",
        json!({"text":"Hello from the agent","utterance_id":"final"}),
    )
    .await;
    assert_eq!(
        duplicate["receipt"], spoke["receipt"],
        "retry reused the receipt without replay"
    );
    let changed_text = call(
        service.socket_path(),
        grant.token(),
        "speak",
        json!({"text":"Different text","utterance_id":"final"}),
    )
    .await;
    assert_eq!(changed_text["ok"], false);
    let spoof_target = call(
        service.socket_path(),
        grant.token(),
        "speak",
        json!({"text":"Wrong target","utterance_id":"other","channel_id":"9"}),
    )
    .await;
    assert_eq!(spoof_target["ok"], false);
    assert!(grant.final_spoken());
    let status = call(
        service.socket_path(),
        grant.token(),
        "voice_status",
        json!({}),
    )
    .await;
    assert_eq!(status["state"], "listening");
    bridge.stop("1", "1:2").await.unwrap();
    let stopped = call(
        service.socket_path(),
        grant.token(),
        "voice_status",
        json!({}),
    )
    .await;
    assert_eq!(stopped["ok"], false);
    drop(grant);
    let expired = call(service.socket_path(), &token, "voice_status", json!({})).await;
    assert_eq!(expired["ok"], false);
    sidecar.await.unwrap();
}

async fn call(path: &std::path::Path, token: &str, method: &str, arguments: Value) -> Value {
    let mut stream = tokio::net::UnixStream::connect(path).await.unwrap();
    let frame = json!({"version":1,"grant":token,"method":method,"arguments":arguments});
    stream
        .write_all(format!("{frame}\n").as_bytes())
        .await
        .unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).await.unwrap();
    serde_json::from_str(&line).unwrap()
}
