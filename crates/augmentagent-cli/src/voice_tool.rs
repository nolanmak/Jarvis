//! Stdio MCP facade for one owner-authorized Discord voice turn.
//! Server credentials and the Unix socket come only from the daemon's MCP
//! configuration; model arguments contain no target guild or channel.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, Result};
use augmentagent_approval_discord::voice_tool::{VoiceToolGrant, VoiceToolService};
use augmentagent_channel_core::reasoner::ReasonerOpts;
use serde_json::{json, Value};

pub fn configure(
    opts: &mut ReasonerOpts,
    grant: &VoiceToolGrant,
    service: &VoiceToolService,
    bin: &Path,
) -> Result<()> {
    let mut settings: Value = opts
        .settings_json
        .as_deref()
        .map(serde_json::from_str)
        .transpose()?
        .unwrap_or_else(|| json!({}));
    settings["mcpServers"]["voice"] = json!({
        "command": bin,
        "args": ["voice-tool"],
        "env": {
            "AUGMENTAGENT_VOICE_TOOL_SOCKET": service.socket_path(),
            "AUGMENTAGENT_VOICE_TOOL_GRANT": grant.token(),
        }
    });
    opts.settings_json = Some(settings.to_string());
    for name in ["speak", "speech_status", "voice_status", "voice_interrupt"] {
        opts.allowed_tools.push(format!("mcp__voice__{name}"));
    }
    opts.system_prompt.push_str("\nDiscord voice is attached to this conversation. You may use mcp__voice__speak with {text, utterance_id} to speak while working. Choose a unique utterance_id per piece of speech; retry the same ID only for the same text. Use utterance_id `final` ONLY when that speech delivers your final response. When `final` is used, the final text response will not be played again. Use speech_status to check a receipt, voice_status to inspect audio, and voice_interrupt to stop playback. These tools are bound to this conversation; never supply a guild or channel ID. Do not speak raw tool logs or private reasoning.\n");
    Ok(())
}

fn call_control(method: &str, arguments: &Value) -> Result<Value> {
    let socket = std::env::var("AUGMENTAGENT_VOICE_TOOL_SOCKET")
        .context("Voice tool socket is not configured")?;
    let grant = std::env::var("AUGMENTAGENT_VOICE_TOOL_GRANT")
        .context("Voice tool grant is not configured")?;
    let mut stream = UnixStream::connect(socket).context("Voice tool socket is unavailable")?;
    stream.set_read_timeout(Some(Duration::from_secs(7)))?;
    stream.set_write_timeout(Some(Duration::from_secs(7)))?;
    let frame = json!({"version":1,"grant":grant,"method":method,"arguments":arguments});
    stream.write_all(frame.to_string().as_bytes())?;
    stream.write_all(b"\n")?;
    let mut response = Vec::new();
    BufReader::new(stream)
        .take(32_769)
        .read_until(b'\n', &mut response)?;
    anyhow::ensure!(
        response.len() <= 32_768 && response.last() == Some(&b'\n'),
        "Voice tool reply is missing or too large"
    );
    let value: Value = serde_json::from_slice(&response)?;
    anyhow::ensure!(
        value["version"] == 1,
        "Unsupported voice tool reply version"
    );
    Ok(value)
}

fn dispatch(req: &Value) -> Value {
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let result = match req["method"].as_str().unwrap_or("") {
        "initialize" => json!({"protocolVersion":"2024-11-05","capabilities":{"tools":{}},
            "serverInfo":{"name":"jarvis-discord-voice","version":"1.0"}}),
        "ping" => json!({}),
        "tools/list" => json!({"tools":[
            {"name":"speak","description":"Speak text in the current Discord voice conversation. The same utterance_id is played at most once; use final only when this delivers the final answer.","inputSchema":{"type":"object","properties":{"text":{"type":"string","maxLength":12000},"utterance_id":{"type":"string","maxLength":64}},"required":["text","utterance_id"],"additionalProperties":false}},
            {"name":"speech_status","description":"Look up a speech receipt from this turn.","inputSchema":{"type":"object","properties":{"utterance_id":{"type":"string"}},"required":["utterance_id"],"additionalProperties":false}},
            {"name":"voice_status","description":"Read the current Discord audio state.","inputSchema":{"type":"object","properties":{},"additionalProperties":false}},
            {"name":"voice_interrupt","description":"Stop the current spoken reply and pending synthesis.","inputSchema":{"type":"object","properties":{},"additionalProperties":false}}
        ]}),
        "tools/call" => {
            let name = req["params"]["name"].as_str().unwrap_or("");
            let args = req["params"]["arguments"].clone();
            let outcome = if matches!(
                name,
                "speak" | "speech_status" | "voice_status" | "voice_interrupt"
            ) {
                call_control(name, &args)
            } else {
                Err(anyhow::anyhow!("Unknown voice tool"))
            };
            match outcome {
                Ok(value) if value["ok"] == true => {
                    json!({"content":[{"type":"text","text":value.to_string()}]})
                }
                Ok(value) => json!({"isError":true,"content":[{"type":"text","text":
                    value["error"].as_str().unwrap_or("Voice tool failed")}] }),
                Err(error) => {
                    json!({"isError":true,"content":[{"type":"text","text":error.to_string()}]})
                }
            }
        }
        _ => {
            return json!({"jsonrpc":"2.0","id":id,
            "error":{"code":-32601,"message":"Method not found"}})
        }
    };
    json!({"jsonrpc":"2.0","id":id,"result":result})
}

pub fn serve() -> Result<()> {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let request: Value = serde_json::from_str(&line?)?;
        if request.get("id").is_none() {
            continue;
        }
        writeln!(out, "{}", dispatch(&request))?;
        out.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_list_only_exposes_conversation_bound_operations() {
        let result = dispatch(&json!({"id":1,"method":"tools/list"}));
        let tools = result["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 4);
        for tool in tools {
            assert!(tool["inputSchema"]["properties"]
                .get("channel_id")
                .is_none());
            assert!(tool["inputSchema"]["properties"].get("guild_id").is_none());
        }
    }
}
