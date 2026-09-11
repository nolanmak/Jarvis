use augmentagent_mcp_memory::{McpRequest, Server};
use augmentagent_store::Store;
use serde_json::{json, Value};

#[test]
fn thread_tool_reads_speakers_and_pages_long_bodies_without_loss() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("data.db");
    let store = Store::open(&db).unwrap();
    let long = "界".repeat(18_000);
    store.with_conn(|conn| {
        for (id, speaker, body, timestamp) in [("one", "+15550000001", long.as_str(), 100), ("two", "me", "yes", 200)] {
            conn.execute("INSERT INTO emails (messageId, threadId, fromEmail, subject, body, firstSeenAt, platform, kind) VALUES (?1,'whatsapp-history:example',?2,'WhatsApp: Example',?3,?4,'whatsapp','dm')", augmentagent_store::rusqlite::params![id,speaker,body,timestamp])?;
        }
        Ok(())
    }).unwrap();
    let server = Server::open(db).unwrap();
    let mut offset = 0;
    let mut body_offset = 0;
    let mut recovered = String::new();
    let mut saw_owner = false;
    for _ in 0..10 {
        let request: McpRequest = serde_json::from_value(json!({"jsonrpc":"2.0", "id":1, "method":"tools/call", "params":{
            "name":"read_conversation_thread", "arguments":{"thread_id":"whatsapp-history:example", "offset":offset,"body_offset":body_offset,"limit":20}
        }})).unwrap();
        let response = server.dispatch(&request);
        assert!(response.get("error").is_none(), "{response}");
        assert_ne!(response["result"]["isError"], true, "{response}");
        let page: Value =
            serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        for message in page["messages"].as_array().unwrap() {
            let text = message["body"].as_str().unwrap();
            assert!(text.chars().count() <= 8_000);
            if message["message_id"] == "one" {
                assert_eq!(message["sender"], "+15550000001");
                recovered.push_str(text);
            } else {
                assert_eq!(message["sender"], "me");
                saw_owner = true;
            }
        }
        let Some(next) = page["next_offset"].as_u64() else {
            break;
        };
        offset = next;
        body_offset = page["next_body_offset"].as_u64().unwrap();
    }
    assert_eq!(recovered, long);
    assert!(saw_owner);
}
