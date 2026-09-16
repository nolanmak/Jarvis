//! Live optional-profile contracts against an authenticated local MCP fixture.
//! No real social accounts or external mutation endpoints are used.
use augmentagent_channel_core::{codex::CodexCliReasoner, reasoner::{self, ClaudeCliReasoner, Reasoner}, tool_audit::AuditLogger};
use serde_json::Value;
use std::{io::{BufRead, BufReader}, path::Path, process::{Child, Command, Stdio}, sync::Arc};

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); }
}

async fn contract(provider: &str) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
    let name = format!("live_{provider}_optional_http_mcp");
    if std::env::var_os("JARVIS_LIVE_MCP_CHILD").is_none() {
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &name, "--ignored", "--nocapture"])
            .current_dir(&root).env("JARVIS_LIVE_MCP_CHILD", "1")
            .env("AUGMENTAGENT_SOCIALAPI_MCP_READONLY", "1")
            .env("SOCIALAPI_API_KEY", "fixture")
            .output().unwrap();
        assert!(output.status.success(), "{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        return;
    }
    let fixture = tempfile::tempdir().unwrap();
    let calls = fixture.path().join("calls.jsonl");
    let script = r#"
import json, sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args): pass
    def do_GET(self):
        self.send_response(405); self.end_headers()
    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        authorized = self.headers.get('Authorization') == 'Bearer fixture'
        with open(sys.argv[1], 'a') as log:
            log.write(json.dumps({'request':request, 'authorized':authorized})+'\n')
        if not authorized:
            self.send_response(401); self.end_headers(); return
        if 'id' not in request:
            self.send_response(202); self.end_headers(); return
        if request['method'] == 'initialize':
            result={'protocolVersion':'2025-03-26','capabilities':{'tools':{}},'serverInfo':{'name':'synthetic-context','version':'1'}}
        elif request['method'] == 'tools/list':
            result={'tools':[{'name':name,'description':description,'inputSchema':{'type':'object','properties':{}}}
                for name,description in [('get_post','Read the synthetic post context.'),('create_post','Create a synthetic post; forbidden by the configured read-only guard.')]]}
        elif request['method'] == 'tools/call':
            result={'content':[{'type':'text','text':'SYNTHETIC_CONTEXT_61D9'}]}
        else:
            result={}
        body=json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}).encode()
        self.send_response(200); self.send_header('Content-Type','application/json')
        self.send_header('Content-Length',str(len(body))); self.end_headers(); self.wfile.write(body)
server=ThreadingHTTPServer(('127.0.0.1',0),Handler)
print(f'http://127.0.0.1:{server.server_port}/mcp',flush=True)
server.serve_forever()
"#;
    let mut server = Server(Command::new("python3").args(["-I", "-u", "-c", script])
        .arg(&calls).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap());
    let mut url = String::new();
    BufReader::new(server.0.stdout.take().unwrap()).read_line(&mut url).unwrap();
    assert!(url.starts_with("http://127.0.0.1:"));
    std::env::set_var("AUGMENTAGENT_SOCIALAPI_MCP_URL", url.trim());
    let wiki = fixture.path().join("wiki");
    std::fs::create_dir(&wiki).unwrap();
    let reasoner: Box<dyn Reasoner> = match provider {
        "codex" => Box::new(CodexCliReasoner::openai()),
        "claude" => Box::new(ClaudeCliReasoner::new()),
        _ => unreachable!(),
    };
    let instruction = "Use the configured read-only socialapi MCP server to retrieve requested context. Return the context marker from its response. Do not create or send anything.";
    let profiles = [
        reasoner::socialapi_draft_opts(instruction.into(), Some(wiki.clone())),
        reasoner::with_socialapi_readonly_mcp(reasoner::draft_opts(instruction.into(), Some(wiki)), &root),
    ];
    for (index, mut opts) in profiles.into_iter().enumerate() {
        let log = fixture.path().join(format!("audit-{index}.jsonl"));
        opts.audit_logger = Some(Arc::new(AuditLogger::new(log.clone())));
        let response = reasoner.call(&opts,
            "Call the configured socialapi get_post tool with empty arguments and return its synthetic context marker. Do not guess the marker or read local files for it. Keep the read-only restriction intact.")
            .await.unwrap();
        assert!(response.contains("SYNTHETIC_CONTEXT_61D9"), "{response}");
        let audit: Vec<Value> = std::fs::read_to_string(log).unwrap().lines()
            .map(|line| serde_json::from_str(line).unwrap()).collect();
        assert!(audit.iter().any(|entry| entry["tool"] == "mcp__socialapi__get_post"), "{audit:?}");
    }
    let received: Vec<Value> = std::fs::read_to_string(calls).unwrap().lines()
        .map(|line| serde_json::from_str(line).unwrap()).collect();
    let operations: Vec<_> = received.iter().filter(|row| row["request"]["method"] == "tools/call").collect();
    assert_eq!(operations.len(), 2, "each profile must reach the integration exactly once");
    assert!(operations.iter().all(|row| row["authorized"] == true && row["request"]["params"]["name"] == "get_post"));
}

#[tokio::test]
#[ignore = "requires Codex login; authenticated local HTTP MCP, no external accounts"]
async fn live_codex_optional_http_mcp() { contract("codex").await; }

#[tokio::test]
#[ignore = "requires Claude login; same local HTTP MCP profiles as Codex"]
async fn live_claude_optional_http_mcp() { contract("claude").await; }
