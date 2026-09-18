//! #1099 / #1098 — the two message tools are listed and round-trip over stdio.

use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::{json, Value};

fn run(db: &std::path::Path, requests: &[Value]) -> Vec<Value> {
    let mut server = Command::new(env!("CARGO_BIN_EXE_augmentagent-mcp-memory"))
        .env("AUGMENTAGENT_DB", db)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = server.stdin.take().unwrap();
    for r in requests {
        writeln!(input, "{r}").unwrap();
    }
    drop(input);
    let out = server.wait_with_output().unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn payload(response: &Value) -> Value {
    serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

fn seed(db: &std::path::Path) {
    use augmentagent_store::{Email, Store};
    let store = Store::open(db).unwrap();
    for (id, from, subject, body, date) in [
        (
            "m1",
            "+14155550123",
            "iMessage: Jane Doe",
            "the budget doc is ready",
            "2026-01-10T10:00:00Z",
        ),
        (
            "m2",
            "me",
            "iMessage: Jane Doe",
            "thanks, reading it now",
            "2026-01-11T10:00:00Z",
        ),
    ] {
        store
            .upsert_email(&Email {
                message_id: id.into(),
                thread_id: Some("imessage:+14155550123".into()),
                from: from.into(),
                to: String::new(),
                cc: String::new(),
                attachments: vec![],
                subject: subject.into(),
                body: body.into(),
                date: date.into(),
                account_entity_id: None,
                platform: "imessage".into(),
                kind: "dm".into(),
            })
            .unwrap();
    }
    augmentagent_messages::index::drain(&store, 100, std::time::Duration::ZERO).unwrap();
}

#[test]
fn both_tools_are_listed_with_schemas() {
    let dir = tempfile::tempdir().unwrap();
    let responses = run(
        &dir.path().join("t.db"),
        &[json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})],
    );
    let tools = responses[0]["result"]["tools"].as_array().unwrap();
    for name in ["search_messages", "conversation_stats"] {
        let tool = tools
            .iter()
            .find(|t| t["name"] == name)
            .unwrap_or_else(|| panic!("{name} not listed"));
        assert!(tool["description"].as_str().unwrap().len() > 50);
        assert_eq!(tool["inputSchema"]["type"], "object");
    }
    // The existing tools are untouched.
    for name in [
        "search_conversation_history",
        "read_conversation_thread",
        "memory_search",
    ] {
        assert!(
            tools.iter().any(|t| t["name"] == name),
            "{name} disappeared"
        );
    }
}

#[test]
fn search_messages_round_trips_and_reports_errors() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.db");
    seed(&db);
    let responses = run(
        &db,
        &[
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"search_messages","arguments":{"query":"budget in:imessage"}}}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"search_messages","arguments":{"query":"frm:someone"}}}),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"search_messages","arguments":{}}}),
        ],
    );
    let hits = payload(&responses[0]);
    assert_eq!(hits["hits"][0]["message_id"], "m1");
    assert_eq!(hits["hits"][0]["conv_kind"], "dm");
    assert_eq!(hits["total_estimate"], 1);
    assert!(responses[1]["error"]["message"]
        .as_str()
        .unwrap()
        .contains("unknown operator"));
    assert!(responses[2]["error"]["message"]
        .as_str()
        .unwrap()
        .contains("query is required"));
}

#[test]
fn conversation_stats_round_trips_and_returns_no_message_text() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.db");
    seed(&db);
    let responses = run(
        &db,
        &[
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"conversation_stats","arguments":{"group_by":"person"}}}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"conversation_stats","arguments":{"group_by":"nonsense"}}}),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"conversation_stats","arguments":{}}}),
        ],
    );
    let stats = payload(&responses[0]);
    let row = &stats["rows"][0];
    assert_eq!(row["key"], "phone:+14155550123");
    assert_eq!(row["messages"], 2);
    assert_eq!(row["from_them"], 1);
    assert_eq!(row["from_me"], 1);
    assert_eq!(row["unresolved"], true);
    let text = serde_json::to_string(&stats).unwrap();
    assert!(!text.contains("budget"), "no message text in stats: {text}");
    assert!(responses[1]["error"]["message"]
        .as_str()
        .unwrap()
        .contains("group_by must be"));
    assert!(responses[2]["error"]["message"]
        .as_str()
        .unwrap()
        .contains("group_by is required"));
}
