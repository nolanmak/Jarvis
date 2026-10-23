//! Bounded stdio MCP facade. Only the daemon supplies the socket and capability;
//! the model receives schemas without routing or credential arguments.
use serde_json::{json, Value};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

const FRAME_LIMIT: u64 = 32_768;

pub fn dispatch(
    request: &Value,
    speech: bool,
    mut control: impl FnMut(&str, &Value) -> io::Result<Value>,
) -> Value {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let result = match request["method"].as_str().unwrap_or("") {
        "initialize" => {
            json!({"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"jarvis-phone","version":"1.0"}})
        }
        "ping" => json!({}),
        "tools/list" => json!({"tools":crate::tools::schemas(speech)}),
        "tools/call" => {
            let name = request["params"]["name"].as_str().unwrap_or("");
            let args = &request["params"]["arguments"];
            if !crate::tools::schemas(speech)
                .iter()
                .any(|tool| tool["name"] == name)
            {
                failure("Unknown phone tool")
            } else {
                match control(name, args) {
                    Ok(reply) if reply["version"] == 1 && reply["ok"] == true && reply.get("result").is_some() =>
                        json!({"content":[{"type":"text","text":reply["result"].to_string()}]}),
                    Ok(reply) if reply["version"] == 1 && reply["ok"] == false =>
                        failure(reply["error"].as_str().unwrap_or("Phone tool failed")),
                    _ => failure("Phone tool unavailable; inspect call status before requesting another call"),
                }
            }
        }
        _ => {
            return json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Method not found"}})
        }
    };
    json!({"jsonrpc":"2.0","id":id,"result":result})
}
fn failure(message: &str) -> Value {
    json!({"isError":true,"content":[{"type":"text","text":message}]})
}

pub fn serve_io(
    input: &mut impl BufRead,
    output: &mut impl Write,
    speech: bool,
    mut control: impl FnMut(&str, &Value) -> io::Result<Value>,
) -> io::Result<()> {
    loop {
        let mut bytes = Vec::new();
        let size = input.take(FRAME_LIMIT + 1).read_until(b'\n', &mut bytes)?;
        if size == 0 {
            return Ok(());
        }
        if size as u64 > FRAME_LIMIT || bytes.last() != Some(&b'\n') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Invalid phone MCP frame",
            ));
        }
        let request: Value = serde_json::from_slice(&bytes)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Invalid phone MCP JSON"))?;
        if request.get("id").is_none() {
            continue;
        }
        writeln!(output, "{}", dispatch(&request, speech, &mut control))?;
        output.flush()?;
    }
}

pub fn serve() -> io::Result<()> {
    let socket = std::env::var_os("AUGMENTAGENT_PHONE_TOOL_SOCKET")
        .ok_or_else(|| io::Error::other("Phone socket is not configured"))?;
    let grant = std::env::var("AUGMENTAGENT_PHONE_TOOL_GRANT")
        .map_err(|_| io::Error::other("Phone grant is not configured"))?;
    let speech = std::env::var("AUGMENTAGENT_PHONE_TOOL_SPEECH").as_deref() == Ok("1");
    serve_io(
        &mut io::stdin().lock(),
        &mut io::stdout().lock(),
        speech,
        |name, args| {
            let mut stream = UnixStream::connect(&socket)?;
            stream.set_read_timeout(Some(Duration::from_secs(50)))?;
            stream.set_write_timeout(Some(Duration::from_secs(5)))?;
            let frame = json!({"version":1,"grant":grant,"method":name,"arguments":args});
            writeln!(stream, "{frame}")?;
            let mut response = Vec::new();
            BufReader::new(stream)
                .take(FRAME_LIMIT + 1)
                .read_until(b'\n', &mut response)?;
            if response.len() as u64 > FRAME_LIMIT || response.last() != Some(&b'\n') {
                return Err(io::Error::other("Invalid phone tool reply"));
            }
            serde_json::from_slice(&response)
                .map_err(|_| io::Error::other("Invalid phone tool reply"))
        },
    )
}
