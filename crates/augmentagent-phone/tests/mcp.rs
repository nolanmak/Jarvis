use augmentagent_phone::mcp;
use serde_json::{json, Value};

#[test]
fn mcp_advertises_only_granted_operations_and_forwards_exact_arguments() {
    for speech in [false, true] {
        let result = mcp::dispatch(&json!({"id":1,"method":"tools/list"}), speech, |_, _| {
            panic!("list must not call daemon")
        });
        let list = result["result"]["tools"].as_array().unwrap();
        assert_eq!(list.len(), if speech { 7 } else { 6 });
        for tool in list {
            let props = &tool["inputSchema"]["properties"];
            for forbidden in [
                "destination",
                "number",
                "code",
                "owner",
                "conversation",
                "webhook",
            ] {
                assert!(props.get(forbidden).is_none());
            }
        }
    }
    let result = mcp::dispatch(
        &json!({"id":"request","method":"tools/call","params":{"name":"call_owner","arguments":{"request_id":"once"}}}),
        false,
        |name, args| {
            assert_eq!(name, "call_owner");
            assert_eq!(args, &json!({"request_id":"once"}));
            Ok(json!({"version":1,"ok":true,"result":{"state":"ringing","id":"call"}}))
        },
    );
    assert_eq!(result["id"], "request");
    let content: Value =
        serde_json::from_str(result["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(content["state"], "ringing");
    for name in ["speak", "dial_arbitrary", "phone_number_verify"] {
        let result = mcp::dispatch(
            &json!({"id":2,"method":"tools/call","params":{"name":name,"arguments":{}}}),
            false,
            |_, _| panic!("ungranted tool forwarded"),
        );
        assert_eq!(result["result"]["isError"], true);
    }
}

#[test]
fn transport_errors_and_invalid_frames_do_not_leak_secrets() {
    let request = json!({"id":1,"method":"tools/call","params":{"name":"phone_status","arguments":{"call_id":"x"}}});
    let result = mcp::dispatch(&request, false, |_, _| {
        Err(std::io::Error::other("SECRET_PATH_OR_TOKEN"))
    });
    assert!(!result.to_string().contains("SECRET"));
    assert_eq!(result["result"]["isError"], true);
    let result = mcp::dispatch(&request, false, |_, _| {
        Ok(json!({"version":99,"ok":true,"result":"SECRET"}))
    });
    assert!(!result.to_string().contains("SECRET"));
    assert_eq!(result["result"]["isError"], true);
    for input in [vec![b'x'; 32769], b"{bad json}\n".to_vec()] {
        let mut output = Vec::new();
        assert!(mcp::serve_io(&mut input.as_slice(), &mut output, false, |_, _| panic!()).is_err());
        assert!(output.is_empty());
    }
    let input = b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n{\"id\":4,\"method\":\"ping\"}\n";
    let mut output = Vec::new();
    mcp::serve_io(&mut input.as_slice(), &mut output, false, |_, _| panic!()).unwrap();
    assert_eq!(String::from_utf8(output).unwrap().lines().count(), 1);
}
