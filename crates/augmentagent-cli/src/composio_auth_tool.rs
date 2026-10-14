//! Owner-authorized MCP facade for starting Composio hosted consent.
//!
//! It deliberately returns a URL instead of opening a browser. The agent can
//! place that URL in its reply, check consent, and explicitly complete the
//! verified connection without requiring a callback to the daemon.

use std::io::{BufRead, Write};
use std::time::Duration;

use anyhow::{Context, Result};
use augmentagent_approval_discord::AuditCtx;
use augmentagent_channel_core::reasoner::ReasonerOpts;
use rusqlite::Connection;
use serde_json::{json, Value};

const MAX_RESPONSE_BYTES: usize = 65_536;

pub fn configure(
    opts: &mut ReasonerOpts,
    ctx: &AuditCtx,
    bin: &std::path::Path,
    db: Option<&std::path::Path>,
) -> Result<()> {
    if !ctx.owner_authorized {
        return Ok(());
    }
    let mut settings: Value = opts
        .settings_json
        .as_deref()
        .map(serde_json::from_str)
        .transpose()?
        .unwrap_or_else(|| json!({}));
    let mut env = serde_json::Map::new();
    if let Some(db) = db {
        let db = if db.is_absolute() {
            db.to_path_buf()
        } else {
            std::env::current_dir()?.join(db)
        };
        env.insert(
            "AUGMENTAGENT_DB".into(),
            Value::String(db.display().to_string()),
        );
    }
    env.insert(
        "DASHBOARD_PORT".into(),
        Value::String(dashboard_port().to_string()),
    );
    if let Ok(key) = std::env::var("AUGMENTAGENT_API_KEY") {
        if !key.is_empty() {
            env.insert("AUGMENTAGENT_API_KEY".into(), Value::String(key));
        }
    }
    settings["mcpServers"]["composio_auth"] = json!({
        "command": bin,
        "args": ["composio-auth-tool"],
        "timeout": 65,
        "env": env,
    });
    opts.settings_json = Some(settings.to_string());
    for tool in ["start_auth", "auth_status", "complete_auth"] {
        opts.allowed_tools
            .push(format!("mcp__composio_auth__{tool}"));
    }
    opts.system_prompt.push_str("\nComposio authentication: when the owner asks to connect Gmail, Google Calendar, Google Drive, or Slack, call mcp__composio_auth__start_auth. Return auth_url verbatim and ask the owner to finish consent and tell you when done. Keep attempt_id for follow-up. Calendar selects the only Gmail entity automatically; if ambiguous, ask which account using the returned entity_id choices. After consent, call auth_status with attempt_id; if ACTIVE, call complete_auth with attempt_id to save the integration. Only claim it is connected when complete_auth returns connected=true. Status checks are live; do not repeat start_auth to check progress. Consent works on another device without a local callback.\n");
    Ok(())
}

fn dashboard_port() -> u16 {
    std::env::var("DASHBOARD_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(3000)
}

// The dashboard may generate its API key on first run and persist it instead
// of receiving AUGMENTAGENT_API_KEY. Read that local daemon secret only in the
// MCP child; it never enters a model argument or a tool response.
fn dashboard_api_key() -> Result<String> {
    if let Ok(key) = std::env::var("AUGMENTAGENT_API_KEY") {
        if !key.is_empty() {
            return Ok(key);
        }
    }
    let db_path = std::env::var("AUGMENTAGENT_DB")
        .context("AUGMENTAGENT_API_KEY or AUGMENTAGENT_DB is required for agent-initiated Composio authentication")?;
    let connection = Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection
        .query_row(
            "SELECT value FROM config WHERE key = 'dashboard_api_key'",
            [],
            |row| row.get(0),
        )
        .context("dashboard API key is unavailable; start the dashboard once or set AUGMENTAGENT_API_KEY")
}

fn validate_args(operation: &str, args: &Value) -> Result<()> {
    let object = args.as_object().context("arguments must be an object")?;
    if operation == "start" {
        anyhow::ensure!(
            object
                .keys()
                .all(|key| key == "provider" || key == "entity_id"),
            "Invalid authentication arguments"
        );
        let provider = args["provider"].as_str().context("provider is required")?;
        anyhow::ensure!(
            matches!(provider, "gmail" | "calendar" | "googledrive" | "slack"),
            "Unsupported provider"
        );
        if let Some(entity) = object.get("entity_id") {
            let entity = entity.as_str().context("entity_id must be a string")?;
            anyhow::ensure!(
                provider == "calendar" && !entity.trim().is_empty() && entity.len() <= 256,
                "entity_id requires calendar and a nonempty account identifier"
            );
        }
    } else {
        anyhow::ensure!(
            object.len() == 1
                && args["attempt_id"]
                    .as_str()
                    .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok()),
            "attempt_id from start_auth is required"
        );
    }
    Ok(())
}

async fn call_dashboard(operation: &str, args: &Value, port: u16, key: &str) -> Result<Value> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(60))
        .connect_timeout(Duration::from_secs(3))
        .build()?;
    let mut response = client
        .post(format!(
            "http://127.0.0.1:{port}/api/composio/auth/{operation}"
        ))
        .bearer_auth(key)
        .json(args)
        .send()
        .await
        .context(
            "dashboard is unavailable; start the dashboard before connecting an integration",
        )?;
    let status = response.status();
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        anyhow::ensure!(
            bytes.len() + chunk.len() <= MAX_RESPONSE_BYTES,
            "dashboard response is too large"
        );
        bytes.extend_from_slice(&chunk);
    }
    let result: Value =
        serde_json::from_slice(&bytes).context("dashboard returned invalid JSON")?;
    anyhow::ensure!(status.is_success(), "{}", result);
    Ok(result)
}

async fn call_auth(operation: &str, args: &Value) -> Result<Value> {
    validate_args(operation, args)?;
    call_dashboard(operation, args, dashboard_port(), &dashboard_api_key()?).await
}

async fn dispatch(req: &Value) -> Value {
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let result = match req["method"].as_str().unwrap_or("") {
        "initialize" => {
            json!({"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"augmentagent-composio-auth","version":"1.0"}})
        }
        "ping" => json!({}),
        "tools/list" => {
            let attempt_schema = json!({"type":"object","properties":{"attempt_id":{"type":"string","description":"The attempt_id returned by start_auth."}},"required":["attempt_id"],"additionalProperties":false});
            json!({"tools":[
                {"name":"start_auth","description":"Create hosted consent for Gmail, Calendar, Drive or Slack; return auth_url to the owner. Consent alone does not save the integration.","inputSchema":{"type":"object","properties":{"provider":{"type":"string","enum":["gmail","calendar","googledrive","slack"]},"entity_id":{"type":"string","description":"Calendar only: select an existing Gmail entity when ambiguous."}},"required":["provider"],"additionalProperties":false}},
                {"name":"auth_status","description":"Read current consent status for an attempt. If ACTIVE, call complete_auth to save it.","inputSchema":attempt_schema},
                {"name":"complete_auth","description":"Verify active consent and save the integration. Safe to retry; never completes inactive consent.","inputSchema":attempt_schema}
            ]})
        }
        "tools/call"
            if matches!(
                req["params"]["name"].as_str(),
                Some("start_auth" | "auth_status" | "complete_auth")
            ) =>
        {
            let operation = match req["params"]["name"].as_str().unwrap() {
                "start_auth" => "start",
                "auth_status" => "status",
                _ => "complete",
            };
            match call_auth(operation, &req["params"]["arguments"]).await {
                Ok(value) => json!({"content":[{"type":"text","text":value.to_string()}]}),
                Err(error) => {
                    json!({"isError":true,"content":[{"type":"text","text":error.to_string()}]})
                }
            }
        }
        "tools/call" => {
            json!({"isError":true,"content":[{"type":"text","text":"Unknown Composio auth tool"}]})
        }
        _ => {
            return json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Method not found"}})
        }
    };
    json!({"jsonrpc":"2.0","id":id,"result":result})
}

pub async fn serve() -> Result<()> {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let request: Value = serde_json::from_str(&line?)?;
        if request.get("id").is_none() {
            continue;
        }
        writeln!(out, "{}", dispatch(&request).await)?;
        out.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_gate_and_database_do_not_depend_on_discord_channel_or_history() {
        let mut ctx = AuditCtx::empty();
        let mut opts = augmentagent_channel_core::reasoner::ask_opts("wiki".into(), "repo".into());
        let original = opts.settings_json.clone();
        let bin = std::path::Path::new("/fixture/augmentagent");
        let db = Some(std::path::Path::new("/fixture/owner.db"));
        configure(&mut opts, &ctx, bin, db).unwrap();
        assert_eq!(opts.settings_json, original);
        assert!(!opts
            .allowed_tools
            .iter()
            .any(|tool| tool.contains("composio_auth")));
        ctx.owner_authorized = true;
        configure(&mut opts, &ctx, bin, db).unwrap();
        let settings: Value = serde_json::from_str(opts.settings_json.as_ref().unwrap()).unwrap();
        assert_eq!(
            settings["mcpServers"]["composio_auth"]["env"]["AUGMENTAGENT_DB"],
            "/fixture/owner.db"
        );
        assert_eq!(
            settings["mcpServers"]["composio_auth"]["command"],
            "/fixture/augmentagent"
        );
        assert_eq!(
            opts.allowed_tools
                .iter()
                .filter(|tool| tool.contains("composio_auth"))
                .count(),
            3
        );
    }

    #[test]
    fn malformed_arguments_fail_before_credentials_or_network_access() {
        for args in [
            json!({"provider":"__proto__"}),
            json!({"provider":"gmail","entity_id":null}),
            json!({"provider":"gmail","entity_id":"other"}),
            json!({"provider":"gmail","extra":true}),
            json!(null),
        ] {
            assert!(validate_args("start", &args).is_err());
        }
        assert!(
            validate_args("start", &json!({"provider":"calendar","entity_id":"owner"})).is_ok()
        );
        assert!(validate_args("status", &json!({"attempt_id":42})).is_err());
    }

    #[tokio::test]
    async fn http_client_handles_chunked_responses_and_custom_ports() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") {
                socket.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            let request = String::from_utf8(request).unwrap().to_lowercase();
            assert!(request.starts_with("post /api/composio/auth/status "));
            assert!(request.contains("authorization: bearer synthetic-key"));
            let length: usize = request
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .unwrap()
                .parse()
                .unwrap();
            let mut body = vec![0; length];
            socket.read_exact(&mut body).unwrap();
            let body: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["attempt_id"], "synthetic-attempt");
            socket.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n12\r\n{\"connected\":true}\r\n0\r\n\r\n").unwrap();
        });
        let value = call_dashboard(
            "status",
            &json!({"attempt_id":"synthetic-attempt"}),
            port,
            "synthetic-key",
        )
        .await
        .unwrap();
        assert_eq!(value["connected"], true);
        server.join().unwrap();
    }

    #[tokio::test]
    async fn mcp_schema_limits_auth_to_supported_composio_toolkits() {
        let response = dispatch(&json!({"id":1,"method":"tools/list"})).await;
        let tool = &response["result"]["tools"][0];
        assert_eq!(tool["name"], "start_auth");
        assert_eq!(
            tool["inputSchema"]["properties"]["provider"]["enum"]
                .as_array()
                .unwrap()
                .len(),
            4
        );
        assert_eq!(tool["inputSchema"]["additionalProperties"], false);
    }
}
