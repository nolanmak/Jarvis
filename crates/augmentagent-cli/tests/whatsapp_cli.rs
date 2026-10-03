use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::{Command, Output};

use augmentagent_store::Store;

fn invoke(db: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_augmentagent"))
        .arg("--db")
        .arg(db)
        .arg("whatsapp")
        .args(args)
        .env("AUGMENTAGENT_WA_SOCK", db.with_extension("missing.sock"))
        .output()
        .unwrap()
}

fn fake_sidecar(
    path: &Path,
    paired: bool,
    pair_success: bool,
) -> std::thread::JoinHandle<Vec<String>> {
    let listener = UnixListener::bind(path).unwrap();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let reader = BufReader::new(stream.try_clone().unwrap());
        let mut seen = Vec::new();
        if !paired {
            writeln!(
                stream,
                "{}",
                serde_json::json!({"version":1,"event":"qr","code":"2@first-test-code"})
            )
            .unwrap();
        }
        for line in reader.lines() {
            let request: serde_json::Value = serde_json::from_str(&line.unwrap()).unwrap();
            let op = request["op"].as_str().unwrap().to_string();
            seen.push(op.clone());
            let id = request["request_id"].as_str().unwrap();
            let result = match op.as_str() {
                "status" => {
                    serde_json::json!({"paired":paired,"connected":paired,"device_jid":if paired {"15551234567:2@s.whatsapp.net"} else {""}})
                }
                "start_pairing" => serde_json::json!({"paired":false}),
                "list_chats" => {
                    serde_json::json!({"chats":[{"jid":"15557654321@s.whatsapp.net","name":"A chat","last_message_at":0}]})
                }
                "logout" => {
                    assert_eq!(
                        request["params"]["expected_device_jid"],
                        "15551234567:2@s.whatsapp.net"
                    );
                    serde_json::json!({"unlinked_device_jid":"15551234567:2@s.whatsapp.net"})
                }
                other => panic!("unexpected sidecar op: {other}"),
            };
            writeln!(
                stream,
                "{}",
                serde_json::json!({"version":1,"request_id":id,"ok":true,"result":result})
            )
            .unwrap();
            if op == "start_pairing" {
                writeln!(
                    stream,
                    "{}",
                    serde_json::json!({"version":1,"event":"qr","code":"2@rotated-test-code"})
                )
                .unwrap();
                if pair_success {
                    writeln!(stream, "{}", serde_json::json!({"version":1,"event":"pair-success","device_jid":"15551234567:2@s.whatsapp.net","user_jid":"15551234567@s.whatsapp.net"})).unwrap();
                }
            }
            if op == "logout" {
                break;
            }
        }
        seen
    })
}

#[test]
fn login_handles_rotated_qr_and_persists_only_the_expected_phone() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("Mac Home ü with spaces");
    std::fs::create_dir(&state).unwrap();
    let db = state.join("data.db");
    let socket = dir.path().join("wa.sock");
    let auth = state.join("auth.json");
    let server = fake_sidecar(&socket, false, true);
    let output = Command::new(env!("CARGO_BIN_EXE_augmentagent"))
        .arg("--db")
        .arg(&db)
        .args([
            "whatsapp",
            "login",
            "--phone",
            "+15551234567",
            "--self-chat",
            "--timeout-secs",
            "5",
        ])
        .env("AUGMENTAGENT_WA_SOCK", &socket)
        .env("AUGMENTAGENT_WHATSAPP_AUTH", &auth)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(server.join().unwrap(), vec!["status", "start_pairing"]);
    let printed = String::from_utf8_lossy(&output.stdout);
    assert_eq!(printed.matches("Scan this QR").count(), 2);
    assert!(!printed.contains("2@first-test-code"));
    assert!(!printed.contains("2@rotated-test-code"));
    let saved = augmentagent_channel_whatsapp::WhatsappAuth::load_from_file(&auth).unwrap();
    assert_eq!(saved.phone, "15551234567");
    assert_eq!(
        Store::open(&db)
            .unwrap()
            .list_active_whatsapp_devices()
            .unwrap()
            .len(),
        1
    );
    let owner = Store::open(&db)
        .unwrap()
        .whatsapp_owner_config("15551234567")
        .unwrap()
        .unwrap();
    assert_eq!(owner.mode, "self_chat");
    assert_eq!(owner.control_chat_jid, "15551234567@s.whatsapp.net");
}

#[test]
fn unlink_calls_sidecar_for_the_selected_device_before_removing_local_state() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("data.db");
    let socket = dir.path().join("wa.sock");
    let auth = dir.path().join("auth.json");
    Store::open(&db)
        .unwrap()
        .upsert_whatsapp_device(
            "15551234567",
            "15551234567:2@s.whatsapp.net",
            "15551234567@s.whatsapp.net",
        )
        .unwrap();
    augmentagent_channel_whatsapp::WhatsappAuth {
        phone: "15551234567".into(),
        device_jid: "15551234567:2@s.whatsapp.net".into(),
        user_jid: "15551234567@s.whatsapp.net".into(),
        paired_at_ms: 1,
    }
    .save_to_file(&auth)
    .unwrap();
    let server = fake_sidecar(&socket, true, false);
    let output = Command::new(env!("CARGO_BIN_EXE_augmentagent"))
        .arg("--db")
        .arg(&db)
        .args(["whatsapp", "unlink", "15551234567"])
        .env("AUGMENTAGENT_WA_SOCK", &socket)
        .env("AUGMENTAGENT_WHATSAPP_AUTH", &auth)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(server.join().unwrap(), vec!["status", "logout"]);
    assert!(!auth.exists());
    assert!(Store::open(&db)
        .unwrap()
        .list_active_whatsapp_devices()
        .unwrap()
        .is_empty());
}

#[test]
fn unlink_is_idempotent_and_never_logs_out_a_different_phone() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("data.db");
    let auth = dir.path().join("missing-auth.json");
    let absent = Command::new(env!("CARGO_BIN_EXE_augmentagent"))
        .arg("--db")
        .arg(&db)
        .args(["whatsapp", "unlink", "15550001111"])
        .env("AUGMENTAGENT_WA_SOCK", dir.path().join("missing.sock"))
        .env("AUGMENTAGENT_WHATSAPP_AUTH", &auth)
        .output()
        .unwrap();
    assert!(
        absent.status.success(),
        "{}",
        String::from_utf8_lossy(&absent.stderr)
    );

    let socket = dir.path().join("wa.sock");
    let server = fake_sidecar(&socket, true, false);
    let wrong = Command::new(env!("CARGO_BIN_EXE_augmentagent"))
        .arg("--db")
        .arg(&db)
        .args(["whatsapp", "unlink", "15550001111"])
        .env("AUGMENTAGENT_WA_SOCK", &socket)
        .env("AUGMENTAGENT_WHATSAPP_AUTH", &auth)
        .output()
        .unwrap();
    assert!(!wrong.status.success());
    assert_eq!(server.join().unwrap(), vec!["status"]);
}

#[test]
fn already_paired_login_reconciles_without_reopening_qr() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("data.db");
    let socket = dir.path().join("wa.sock");
    let auth = dir.path().join("auth.json");
    let server = fake_sidecar(&socket, true, false);
    let output = Command::new(env!("CARGO_BIN_EXE_augmentagent"))
        .arg("--db")
        .arg(&db)
        .args(["whatsapp", "login", "--phone", "15551234567", "--self-chat"])
        .env("AUGMENTAGENT_WA_SOCK", &socket)
        .env("AUGMENTAGENT_WHATSAPP_AUTH", &auth)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(server.join().unwrap(), vec!["status"]);
    assert!(auth.exists());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("Scan this QR"));
}

#[test]
fn credential_store_failure_does_not_create_a_local_device_row() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("data.db");
    let socket = dir.path().join("wa.sock");
    let server = fake_sidecar(&socket, true, false);
    let output = Command::new(env!("CARGO_BIN_EXE_augmentagent"))
        .arg("--db")
        .arg(&db)
        .args(["whatsapp", "login", "--phone", "15551234567", "--self-chat"])
        .env("AUGMENTAGENT_WA_SOCK", &socket)
        .env("AUGMENTAGENT_WHATSAPP_AUTH", "/dev/null/auth.json")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(server.join().unwrap(), vec!["status"]);
    assert!(Store::open(&db)
        .unwrap()
        .list_active_whatsapp_devices()
        .unwrap()
        .is_empty());
}

#[test]
fn unlink_then_repair_restores_one_device_and_owner_binding() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("data.db");
    let auth = dir.path().join("auth.json");
    let run = |socket: &Path, args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_augmentagent"))
            .arg("--db")
            .arg(&db)
            .arg("whatsapp")
            .args(args)
            .env("AUGMENTAGENT_WA_SOCK", socket)
            .env("AUGMENTAGENT_WHATSAPP_AUTH", &auth)
            .output()
            .unwrap()
    };
    let first_socket = dir.path().join("first.sock");
    let first = fake_sidecar(&first_socket, true, false);
    assert!(run(
        &first_socket,
        &["login", "--phone", "15551234567", "--self-chat"]
    )
    .status
    .success());
    first.join().unwrap();
    let unlink_socket = dir.path().join("unlink.sock");
    let linked = fake_sidecar(&unlink_socket, true, false);
    assert!(run(&unlink_socket, &["unlink", "15551234567"])
        .status
        .success());
    linked.join().unwrap();
    let repair_socket = dir.path().join("repair.sock");
    let repairing = fake_sidecar(&repair_socket, false, true);
    let repaired = run(
        &repair_socket,
        &[
            "login",
            "--phone",
            "15551234567",
            "--self-chat",
            "--timeout-secs",
            "5",
        ],
    );
    assert!(
        repaired.status.success(),
        "{}",
        String::from_utf8_lossy(&repaired.stderr)
    );
    repairing.join().unwrap();
    let store = Store::open(&db).unwrap();
    assert_eq!(store.list_active_whatsapp_devices().unwrap().len(), 1);
    assert_eq!(
        store
            .whatsapp_owner_config("15551234567")
            .unwrap()
            .unwrap()
            .mode,
        "self_chat"
    );
    assert!(auth.exists());
}

#[test]
fn dedicated_account_records_a_distinct_owner_control_chat() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("data.db");
    let socket = dir.path().join("wa.sock");
    let auth = dir.path().join("auth.json");
    let server = fake_sidecar(&socket, true, false);
    let output = Command::new(env!("CARGO_BIN_EXE_augmentagent"))
        .arg("--db")
        .arg(&db)
        .args([
            "whatsapp",
            "login",
            "--phone",
            "15551234567",
            "--owner-jid",
            "15557654321@s.whatsapp.net",
        ])
        .env("AUGMENTAGENT_WA_SOCK", &socket)
        .env("AUGMENTAGENT_WHATSAPP_AUTH", &auth)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(server.join().unwrap(), vec!["status"]);
    let config = Store::open(&db)
        .unwrap()
        .whatsapp_owner_config("15551234567")
        .unwrap()
        .unwrap();
    assert_eq!(config.owner_jid, "15557654321@s.whatsapp.net");
    assert_eq!(config.control_chat_jid, config.owner_jid);
    assert_eq!(config.mode, "dedicated");
}

#[test]
fn login_timeout_and_wrong_phone_leave_no_local_binding() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("data.db");
    let socket = dir.path().join("wa.sock");
    let auth = dir.path().join("auth.json");
    let server = fake_sidecar(&socket, false, false);
    let timed_out = Command::new(env!("CARGO_BIN_EXE_augmentagent"))
        .arg("--db")
        .arg(&db)
        .args([
            "whatsapp",
            "login",
            "--phone",
            "15551234567",
            "--self-chat",
            "--timeout-secs",
            "1",
        ])
        .env("AUGMENTAGENT_WA_SOCK", &socket)
        .env("AUGMENTAGENT_WHATSAPP_AUTH", &auth)
        .output()
        .unwrap();
    assert!(!timed_out.status.success());
    assert!(String::from_utf8_lossy(&timed_out.stderr).contains("timed out"));
    assert_eq!(server.join().unwrap(), vec!["status", "start_pairing"]);
    assert!(!auth.exists());
    assert!(Store::open(&db)
        .unwrap()
        .list_active_whatsapp_devices()
        .unwrap()
        .is_empty());

    let second_socket = dir.path().join("second.sock");
    let second = fake_sidecar(&second_socket, true, false);
    let wrong = Command::new(env!("CARGO_BIN_EXE_augmentagent"))
        .arg("--db")
        .arg(&db)
        .args(["whatsapp", "login", "--phone", "15550001111", "--self-chat"])
        .env("AUGMENTAGENT_WA_SOCK", &second_socket)
        .env("AUGMENTAGENT_WHATSAPP_AUTH", &auth)
        .output()
        .unwrap();
    assert!(!wrong.status.success());
    assert_eq!(second.join().unwrap(), vec!["status"]);
    assert!(!auth.exists());
    assert!(Store::open(&db)
        .unwrap()
        .list_active_whatsapp_devices()
        .unwrap()
        .is_empty());
}

#[test]
fn ctrl_c_cancels_pairing_without_persisting_a_device() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("data.db");
    let socket = dir.path().join("wa.sock");
    let auth = dir.path().join("auth.json");
    let server = fake_sidecar(&socket, false, false);
    let mut child = Command::new(env!("CARGO_BIN_EXE_augmentagent"))
        .arg("--db")
        .arg(&db)
        .args([
            "whatsapp",
            "login",
            "--phone",
            "15551234567",
            "--self-chat",
            "--timeout-secs",
            "10",
        ])
        .env("AUGMENTAGENT_WA_SOCK", &socket)
        .env("AUGMENTAGENT_WHATSAPP_AUTH", &auth)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let (ready, receiver) = std::sync::mpsc::channel();
    let stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if line.unwrap().contains("Scan this QR") {
                let _ = ready.send(());
            }
        }
    });
    if receiver
        .recv_timeout(std::time::Duration::from_secs(5))
        .is_err()
    {
        let _ = child.kill();
        let _ = child.wait();
        panic!("pairing never displayed a QR");
    }
    unsafe {
        libc::kill(child.id() as i32, libc::SIGINT);
    }
    let output = child.wait_with_output().unwrap();
    reader.join().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("pairing cancelled"));
    assert_eq!(server.join().unwrap(), vec!["status", "start_pairing"]);
    assert!(!auth.exists());
    assert!(Store::open(&db)
        .unwrap()
        .list_active_whatsapp_devices()
        .unwrap()
        .is_empty());
}

#[test]
fn status_and_devices_are_machine_readable_without_a_sidecar() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("data.db");
    let status = invoke(&db, &["status"]);
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(json["sidecar_running"], false);
    assert_eq!(json["paired"], false);
    assert_eq!(json["connected"], false);
    let devices = invoke(&db, &["devices", "--json"]);
    assert!(
        devices.status.success(),
        "{}",
        String::from_utf8_lossy(&devices.stderr)
    );
    let rows: serde_json::Value = serde_json::from_slice(&devices.stdout).unwrap();
    assert!(rows.as_array().unwrap().is_empty());
}

#[test]
fn status_distinguishes_paired_connected_and_owner_configured() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("data.db");
    let socket = dir.path().join("wa.sock");
    let store = Store::open(&db).unwrap();
    store
        .upsert_whatsapp_device(
            "15551234567",
            "15551234567:2@s.whatsapp.net",
            "15551234567@s.whatsapp.net",
        )
        .unwrap();
    store
        .set_whatsapp_owner_config(&augmentagent_store::WhatsappOwnerConfig {
            phone: "15551234567".into(),
            owner_jid: "15557654321@s.whatsapp.net".into(),
            control_chat_jid: "15557654321@s.whatsapp.net".into(),
            mode: "dedicated".into(),
        })
        .unwrap();
    let server = fake_sidecar(&socket, true, false);
    let output = Command::new(env!("CARGO_BIN_EXE_augmentagent"))
        .arg("--db")
        .arg(&db)
        .args(["whatsapp", "status"])
        .env("AUGMENTAGENT_WA_SOCK", &socket)
        .env_remove("AUGMENTAGENT_WHATSAPP_CONTROL_ENABLED")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(server.join().unwrap(), vec!["status"]);
    let status: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(status["sidecar_running"], true);
    assert_eq!(status["paired"], true);
    assert_eq!(status["connected"], true);
    assert_eq!(status["configured"], true);
    assert_eq!(status["enabled"], false);
    assert_eq!(status["actively_listening"], false);
    assert_eq!(status["owner_jid"], "15557654321@s.whatsapp.net");
}

#[test]
fn list_chats_reads_the_sidecar_without_claiming_archived_history() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("data.db");
    let socket = dir.path().join("wa.sock");
    let server = fake_sidecar(&socket, true, false);
    let output = Command::new(env!("CARGO_BIN_EXE_augmentagent"))
        .arg("--db")
        .arg(&db)
        .args(["whatsapp", "list-chats", "--json"])
        .env("AUGMENTAGENT_WA_SOCK", &socket)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(server.join().unwrap(), vec!["list_chats"]);
    let chats: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(chats[0]["jid"], "15557654321@s.whatsapp.net");
}

#[test]
fn allowlist_commands_persist_valid_jids_and_reject_invalid_ones() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("data.db");
    let jid = "15551234567@s.whatsapp.net";
    for args in [vec!["allow-inbound", jid], vec!["allow-outbound", jid]] {
        let output = invoke(&db, &args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let store = Store::open(&db).unwrap();
    assert!(store.is_whatsapp_inbound_allowed(jid).unwrap());
    assert!(store.is_whatsapp_outbound_allowed(jid).unwrap());
    let invalid = invoke(&db, &["allow-outbound", "not-a-jid"]);
    assert!(!invalid.status.success());
    assert!(!String::from_utf8_lossy(&invalid.stderr).contains("panicked"));
    assert!(!store.is_whatsapp_outbound_allowed("not-a-jid").unwrap());
    for args in [vec!["deny-inbound", jid], vec!["deny-outbound", jid]] {
        let output = invoke(&db, &args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(!store.is_whatsapp_inbound_allowed(jid).unwrap());
    assert!(!store.is_whatsapp_outbound_allowed(jid).unwrap());
}

#[test]
fn subscription_requires_the_sidecars_actual_paired_device() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("data.db");
    let socket = dir.path().join("wa.sock");
    let chat = "15557654321@s.whatsapp.net";
    let store = Store::open(&db).unwrap();
    store
        .upsert_whatsapp_device(
            "15551234567",
            "15551234567:2@s.whatsapp.net",
            "15551234567@s.whatsapp.net",
        )
        .unwrap();
    let disconnected = invoke(&db, &["subscribe", chat, "--mode", "priority"]);
    assert!(!disconnected.status.success());
    assert!(store
        .list_active_subscriptions("whatsapp")
        .unwrap()
        .is_empty());
    let group = invoke(
        &db,
        &["subscribe", "12345-67890@g.us", "--mode", "priority"],
    );
    assert!(
        !group.status.success(),
        "groups cannot be silently subscribed while the channel drops them"
    );
    assert!(String::from_utf8_lossy(&group.stderr).contains("group"));

    let server = fake_sidecar(&socket, true, false);
    let subscribed = Command::new(env!("CARGO_BIN_EXE_augmentagent"))
        .arg("--db")
        .arg(&db)
        .args([
            "whatsapp",
            "subscribe",
            chat,
            "--mode",
            "priority",
            "--name",
            "A chat",
        ])
        .env("AUGMENTAGENT_WA_SOCK", &socket)
        .output()
        .unwrap();
    assert!(
        subscribed.status.success(),
        "{}",
        String::from_utf8_lossy(&subscribed.stderr)
    );
    assert_eq!(server.join().unwrap(), vec!["status"]);
    let sub: serde_json::Value = serde_json::from_slice(&subscribed.stdout).unwrap();
    assert_eq!(sub["account_id"], "15551234567");
    assert_eq!(sub["channel_id"], chat);
    let listed = invoke(&db, &["subscriptions", "--json"]);
    assert!(listed.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&listed.stdout)
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let id = sub["id"].as_str().unwrap();
    assert!(invoke(&db, &["unsubscribe", id]).status.success());
    assert!(store
        .list_active_subscriptions("whatsapp")
        .unwrap()
        .is_empty());
}

#[test]
fn serve_processes_owner_self_chat_immediately_and_ignores_echoes_and_strangers() {
    use std::{process::Stdio, sync::mpsc, time::Duration};
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("agent.db");
    let socket = dir.path().join("wa.sock");
    let store = Store::open(&db).unwrap();
    store
        .upsert_whatsapp_device(
            "15551234567",
            "15551234567:2@s.whatsapp.net",
            "15551234567@s.whatsapp.net",
        )
        .unwrap();
    store
        .set_whatsapp_owner_config(&augmentagent_store::WhatsappOwnerConfig {
            phone: "15551234567".into(),
            owner_jid: "15551234567@s.whatsapp.net".into(),
            control_chat_jid: "15551234567@s.whatsapp.net".into(),
            mode: "self_chat".into(),
        })
        .unwrap();
    store
        .allow_whatsapp_outbound("15551234567@s.whatsapp.net")
        .unwrap();
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let (sent, received) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(c) => break c,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "daemon did not connect"
                    );
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => panic!("{e}"),
            }
        };
        // Darwin inherits O_NONBLOCK from the listener; Linux does not.
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        let reader = BufReader::new(stream.try_clone().unwrap());
        let mut replayed = false;
        let mut statuses = 0;
        for line in reader.lines() {
            let line = match line {
                Ok(line) => line,
                Err(_) => break,
            };
            let request: serde_json::Value = serde_json::from_str(&line).unwrap();
            let result = match request["op"].as_str().unwrap() {
                "status" => {
                    statuses += 1;
                    serde_json::json!({"paired":true,"connected":statuses > 2,"device_jid":"15551234567:2@s.whatsapp.net"})
                }
                "replay_events" if !replayed => {
                    replayed = true;
                    let events:Vec<_>=(1..=3).map(|seq|serde_json::json!({"version":1,"event":"received-message",
                        "seq":seq,"id":format!("m{seq}"),"chat":"15551234567@s.whatsapp.net",
                        "sender":if seq==1{"15550000000@s.whatsapp.net"}else{"15551234567:9@s.whatsapp.net"},
                        "text":"help","timestamp":1,"from_me":true,"origin_verified":true,
                        "agent_generated":seq==2,"account_jid":"15551234567:2@s.whatsapp.net"})).collect();
                    serde_json::json!({"events":events})
                }
                "replay_events" => serde_json::json!({"events":[]}),
                "ack_events" => serde_json::json!({"acked_through":request["params"]["through"]}),
                "send_text" => {
                    sent.send(request["params"].clone()).unwrap();
                    serde_json::json!({"message_id":"agent-reply"})
                }
                other => panic!("unexpected op {other}"),
            };
            if writeln!(stream,"{}",serde_json::json!({"version":1,"request_id":request["request_id"],"ok":true,"result":result})).is_err(){break;}
        }
    });
    struct ChildGuard(std::process::Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let stderr = std::fs::File::create(dir.path().join("daemon.log")).unwrap();
    let mut daemon = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_augmentagent"))
            .current_dir(dir.path())
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", dir.path().join("home"))
            .env("XDG_STATE_HOME", dir.path().join("state"))
            .env(
                "AUGMENTAGENT_INSECURE_CREDENTIAL_DIR",
                dir.path().join("credentials"),
            )
            .env("AUGMENTAGENT_WA_SOCK", &socket)
            .env("AUGMENTAGENT_WHATSAPP_CONTROL_ENABLED", "1")
            .env("AUGMENTAGENT_SLACK_INTERACTIVE", "0")
            .env("AUGMENTAGENT_GH_DISABLE", "1")
            .env("AUGMENTAGENT_REASONER_CHAIN", "claude")
            .env("CLAUDE_CLI", "/nonexistent/claude")
            .arg("--db")
            .arg(&db)
            .arg("--wiki-dir")
            .arg(dir.path().join("wiki"))
            .args(["serve", "--no-email", "true", "--dry-run", "false"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()
            .unwrap(),
    );
    let reply = received
        .recv_timeout(Duration::from_secs(15))
        .unwrap_or_else(|e| {
            panic!(
                "no immediate reply: {e}: {}",
                std::fs::read_to_string(dir.path().join("daemon.log")).unwrap()
            )
        });
    assert_eq!(reply["chat_jid"], "15551234567@s.whatsapp.net");
    assert_eq!(
        reply["idempotency_key"],
        "reply:15551234567@s.whatsapp.net:m3:0"
    );
    assert!(reply["text"].as_str().unwrap().contains("Send a question"));
    assert!(
        received.recv_timeout(Duration::from_millis(1500)).is_err(),
        "echo or stranger got a reply"
    );
    assert!(store
        .pending_whatsapp_control("15551234567", "15551234567@s.whatsapp.net")
        .unwrap()
        .is_empty());
    unsafe {
        libc::kill(daemon.0.id() as i32, libc::SIGTERM);
    }
    for _ in 0..50 {
        if daemon.0.try_wait().unwrap().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    drop(daemon);
    server.join().unwrap();
}
