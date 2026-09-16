use serde_json::{json, Value};
use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn initialized_notification_has_no_response_and_does_not_poison_next_request() {
    let fixture = tempfile::tempdir().unwrap();
    let mut server = Command::new(env!("CARGO_BIN_EXE_augmentagent-mcp-memory"))
        .env("AUGMENTAGENT_DB", fixture.path().join("synthetic.db"))
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().unwrap();
    let mut input = server.stdin.take().unwrap();
    for request in [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
        json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":99}}),
        json!({"jsonrpc":"2.0","method":"tools/call","params":{"name":"memory_write","arguments":{"surface":"synthetic","subject":"must not run","body":"SYNTHETIC"}}}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"memory_recent","arguments":{"limit":1}}}),
    ] {
        writeln!(input, "{request}").unwrap();
    }
    drop(input);
    let output = server.wait_with_output().unwrap();
    assert!(output.status.success());
    let responses: Vec<Value> = String::from_utf8(output.stdout).unwrap().lines()
        .map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(responses.len(), 3, "notifications must not produce JSON-RPC responses");
    for (index, response) in responses.iter().enumerate() {
        assert_eq!(response["id"], index + 1);
        assert!(response.get("error").is_none(), "{response}");
    }
    assert!(responses[1]["result"]["tools"].as_array().unwrap().iter()
        .any(|tool| tool["name"] == "memory_recent"));
    assert_ne!(responses[2]["result"]["isError"], true);
    let recent: Value = serde_json::from_str(responses[2]["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(recent["hits"], json!([]), "id-less tool calls must not mutate memory");
}

#[test]
fn invalid_request_ids_and_versions_cannot_mutate_memory() {
    let fixture = tempfile::tempdir().unwrap();
    let mut server = Command::new(env!("CARGO_BIN_EXE_augmentagent-mcp-memory"))
        .env("AUGMENTAGENT_DB", fixture.path().join("synthetic.db"))
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().unwrap();
    let mut input = server.stdin.take().unwrap();
    let args = json!({"name":"memory_write","arguments":{"surface":"synthetic","subject":"must not run","body":"SYNTHETIC"}});
    for id in [Value::Null, json!(true), json!(1.5), json!([]), json!({})] {
        writeln!(input, "{}", json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":args})).unwrap();
    }
    writeln!(input, "{}", json!({"jsonrpc":"1.0","method":"tools/call","params":args})).unwrap();
    writeln!(input, "{}", json!({"jsonrpc":"1.0","id":7,"method":"tools/call","params":args})).unwrap();
    writeln!(input, "{{malformed").unwrap();
    writeln!(input, "{}", json!({"jsonrpc":"2.0","id":"readback","method":"tools/call","params":{"name":"memory_recent","arguments":{"limit":10}}})).unwrap();
    drop(input);
    let output = server.wait_with_output().unwrap();
    assert!(output.status.success());
    let responses: Vec<Value> = String::from_utf8(output.stdout).unwrap().lines()
        .map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(responses.len(), 9);
    for response in &responses[..7] { assert_eq!(response["error"]["code"], -32600, "{response}"); }
    assert_eq!(responses[7]["error"]["code"], -32700);
    assert_eq!(responses[8]["id"], "readback");
    let recent: Value = serde_json::from_str(responses[8]["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(recent["hits"], json!([]));
}
