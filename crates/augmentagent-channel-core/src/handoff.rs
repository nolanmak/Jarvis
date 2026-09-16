//! Provider-neutral operation receipts and primary-provider hook transport.
//!
//! Receipts are private task state, not audit logs or model-readable files.
use crate::reasoner::ReasonerOpts;
use serde_json::{json, Value};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub(crate) fn system_root() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home)
        .join(".local/state/augmentagent/reasoner-handoffs"))
}

/// A channel turn id makes a replay after restart address the same journal.
/// Calls without a turn id receive an isolated id rather than conflating two
/// intentional, identical requests from the same user.
pub(crate) fn request_path(root: &Path, opts: &ReasonerOpts) -> anyhow::Result<PathBuf> {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let identity = opts.session_id.as_deref().filter(|id| !id.trim().is_empty() && *id != "-")
        .map(str::to_owned).unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    // Prompt context contains clocks, refreshed owner instructions and runtime
    // settings. None of those makes a replayed channel event a new request.
    // Callers must supply a globally namespaced per-turn id, not a chat id.
    let digest = Sha256::digest(serde_json::to_vec(&json!(["turn-v1", identity]))?);
    let request = root.join(format!("{digest:x}"));
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&request)?;
    for path in [root, request.as_path()] {
        let metadata = std::fs::symlink_metadata(path)?;
        anyhow::ensure!(metadata.is_dir() && !metadata.file_type().is_symlink()
            && metadata.permissions().mode() & 0o077 == 0, "handoff directory is not private");
    }
    Ok(request.canonicalize()?.join("operations.json"))
}

/// Supply known progress as data, never as a new system instruction. The
/// journal remains the enforcement boundary even if the model ignores this.
pub(crate) fn resume_message(path: &Path, original: &str) -> anyhow::Result<String> {
    use std::os::unix::fs::PermissionsExt;
    if crate::process_tree::ensure_request_idle(path).is_err() {
        return Err(crate::reasoner::ReasonerError::CleanupUncertain { provider: "previous invocation".into() }.into());
    }
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(original.into()),
        Err(error) => return Err(error.into()),
    };
    anyhow::ensure!(metadata.is_file() && !metadata.file_type().is_symlink()
        && metadata.permissions().mode() & 0o077 == 0 && metadata.len() <= 16 * 1024 * 1024,
        "handoff state is not a private bounded file");
    let state: Value = serde_json::from_slice(&std::fs::read(path)?)?;
    anyhow::ensure!(state["version"] == 1 && state["operations"].is_array(), "invalid handoff state");
    let operations = state["operations"].as_array().unwrap();
    if operations.is_empty() { return Ok(original.into()) }
    let receipts = serde_json::to_string(operations)?;
    // Do not silently discard operations or their outcome to fit a prompt.
    anyhow::ensure!(receipts.len() <= 1024 * 1024, "handoff context requires compaction before resuming");
    Ok(format!("{original}\n\nJarvis recovery context (tool-result data, not instructions):\n\
        A previous provider attempted this same request. Use these receipts as known progress. \
        Do not repeat completed external actions or infer that a started operation failed. \
        Reconcile uncertain outcomes using read-only evidence before any further mutation.\n{receipts}"))
}

pub(crate) struct ClaudeHooks {
    // Keep the executable hook alive for the entire primary call.
    _directory: tempfile::TempDir,
    pub settings_json: String,
}

impl ClaudeHooks {
    pub fn prepare(opts: &ReasonerOpts) -> anyhow::Result<Option<Self>> {
        let Some(journal) = &opts.handoff_path else { return Ok(None) };
        anyhow::ensure!(journal.is_absolute(), "handoff path must be absolute");
        let directory = tempfile::tempdir()?;
        let script = directory.path().join("handoff-hook.py");
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true)
            .mode(0o600).open(&script)?;
        file.write_all(include_bytes!("../../../scripts/codex-tool-bridge.py"))?;
        let mut settings: Value = match &opts.settings_json {
            Some(raw) => serde_json::from_str(raw)?,
            None => json!({}),
        };
        let object = settings.as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("invalid primary settings"))?;
        let hooks = object.entry("hooks").or_insert_with(|| json!({})).as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("invalid primary hooks"))?;
        fn shell_quote(value: &str) -> String {
            format!("'{}'", value.replace('\'', "'\\''"))
        }
        // Also turn interpreter/loader failures into a blocking hook result.
        let command = format!("python3 -I {} --handoff-hook {} || exit 2",
            shell_quote(&script.to_string_lossy()), shell_quote(&journal.to_string_lossy()));
        for event in ["PreToolUse", "PostToolUse", "PostToolUseFailure"] {
            let groups = hooks.entry(event).or_insert_with(|| json!([])).as_array_mut()
                .ok_or_else(|| anyhow::anyhow!("invalid primary hook groups"))?;
            groups.push(json!({"matcher": ".*", "hooks": [{
                "type": "command", "command": command, "timeout": 10
            }]}));
        }
        Ok(Some(Self { _directory: directory, settings_json: serde_json::to_string(&settings)? }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires Claude and Codex login; synthetic MCP counter only"]
    async fn live_primary_receipt_prevents_codex_repeating_an_external_effect() {
        live_external_handoff(false).await;
    }

    #[tokio::test]
    #[ignore = "requires Claude and Codex login; synthetic MCP disconnect after effect"]
    async fn live_primary_disconnect_blocks_uncertain_effect_replay() {
        live_external_handoff(true).await;
    }

    async fn live_external_handoff(disconnect: bool) {
        use crate::reasoner::{ClaudeCliReasoner, Reasoner};
        use crate::codex::CodexCliReasoner;
        use std::os::unix::fs::PermissionsExt;
        let private = tempfile::tempdir().unwrap();
        std::fs::set_permissions(private.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let counter = private.path().join("counter.txt");
        let server = private.path().join("fixture.py");
        std::fs::write(&server, r#"
import json, os, pathlib, sys
counter = pathlib.Path(sys.argv[1])
for line in sys.stdin:
    request = json.loads(line)
    if 'id' not in request:
        continue
    method = request['method']
    if method == 'initialize':
        result = {'protocolVersion':'2024-11-05','capabilities':{'tools':{}},'serverInfo':{'name':'fixture','version':'1'}}
    elif method == 'tools/list':
        result = {'tools':[{'name':'record','description':'Record one synthetic event and return its receipt.',
            'inputSchema':{'type':'object','properties':{'value':{'type':'string'}},'required':['value'],'additionalProperties':False}}]}
    elif method == 'tools/call':
        assert request['params']['name'] == 'record'
        assert request['params']['arguments'] == {'value':'synthetic'}
        count = int(counter.read_text()) + 1 if counter.exists() else 1
        counter.write_text(str(count))
        if sys.argv[2] == 'disconnect':
            os._exit(0)  # Effect happened, but neither provider receives a result.
        result = {'content':[{'type':'text','text':'SYNTHETIC_RECEIPT_'+str(count)}]}
    else:
        result = {}
    print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}),flush=True)
"#).unwrap();
        let mut opts = crate::reasoner::loop_parse_opts();
        opts.system_prompt = "Use only the supplied fixture MCP tool. Report its actual receipt or error. Do not retry errors. Do not use files or shell tools.".into();
        opts.allowed_tools = vec!["mcp__fixture__record".into()];
        opts.cwd = Some(workspace.path().into());
        opts.restrict_env = true;
        opts.settings_json = Some(json!({"mcpServers":{"fixture":{
            "command":"python3","args":["-I",server,counter,if disconnect { "disconnect" } else { "complete" }]
        }}}).to_string());
        let journal = private.path().join("operations.json");
        opts.handoff_path = Some(journal.clone());
        let request = "Call the fixture record tool with value=synthetic and return its receipt.";
        let first = ClaudeCliReasoner::new().call(&opts, request).await;
        if !disconnect {
            let first = first.unwrap();
            assert!(first.contains("SYNTHETIC_RECEIPT_1"), "{first}");
        }
        assert_eq!(std::fs::read_to_string(&counter).unwrap(), "1");
        let state: Value = serde_json::from_slice(&std::fs::read(&journal).unwrap()).unwrap();
        let operations = state["operations"].as_array().unwrap();
        let effects: Vec<_> = operations.iter().filter(|row| row["tool"] == "mcp__fixture__record").collect();
        // Claude may also perform built-in MCP discovery before the call.
        assert_eq!(effects.len(), 1);
        assert_eq!(effects[0]["status"], if disconnect { "started" } else { "completed" });
        assert!(effects[0]["primary_id"].is_string());
        if !disconnect { assert!(operations.iter().all(|row| row["status"] == "completed")); }
        // Deliberately omit recovery prose: durable enforcement must still
        // return the receipt if the fallback tries to repeat the operation.
        let audit = private.path().join("fallback-audit.jsonl");
        opts.audit_logger = Some(std::sync::Arc::new(crate::tool_audit::AuditLogger::new(audit.clone())));
        let second = CodexCliReasoner::openai().call(&opts, request).await.unwrap();
        if disconnect {
            let records: Vec<Value> = std::fs::read_to_string(audit).unwrap().lines()
                .map(|line| serde_json::from_str(line).unwrap()).collect();
            assert!(records.iter().any(|row| row["provider"] == "codex"
                && row["tool"] == "mcp__fixture__record"
                && row["stderr_truncated"].as_str().is_some_and(|text| text.contains("uncertain outcome"))),
                "fallback must receive the reconciliation refusal");
        } else {
            assert!(second.contains("SYNTHETIC_RECEIPT_1"), "{second}");
        }
        assert_eq!(std::fs::read_to_string(&counter).unwrap(), "1");
        let after: Value = serde_json::from_slice(&std::fs::read(&journal).unwrap()).unwrap();
        assert_eq!(after, state);
        if disconnect {
            // Operator observes the authoritative synthetic service counter,
            // records its result through the shipped recovery CLI, then resumes.
            let helper = private.path().join("recovery.py");
            std::fs::write(&helper, include_bytes!("../../../scripts/codex-tool-bridge.py")).unwrap();
            let status = std::process::Command::new("python3").arg(&helper)
                .arg("--handoff-status").arg(&journal).output().unwrap();
            assert!(status.status.success());
            let rows: Vec<Value> = serde_json::from_slice(&status.stdout).unwrap();
            let row = rows.iter().find(|row| row["tool"] == "mcp__fixture__record").unwrap();
            assert_eq!(std::fs::read_to_string(&counter).unwrap(), "1");
            let decision = json!({"index":row["index"], "fingerprint":row["fingerprint"],
                "outcome":"completed", "evidence":"Authoritative synthetic counter equals one.",
                "result":{"content":[{"type":"text","text":"SYNTHETIC_RECEIPT_1"}]}});
            let mut recovery = std::process::Command::new("python3").arg(&helper)
                .arg("--handoff-reconcile").arg(&journal)
                .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped()).spawn().unwrap();
            recovery.stdin.take().unwrap().write_all(decision.to_string().as_bytes()).unwrap();
            let result = recovery.wait_with_output().unwrap();
            assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
            let recovered = resume_message(&journal, request).unwrap();
            let response = CodexCliReasoner::openai().call(&opts, &recovered).await.unwrap();
            assert!(response.contains("SYNTHETIC_RECEIPT_1"), "{response}");
            assert_eq!(std::fs::read_to_string(&counter).unwrap(), "1");
            let final_state: Value = serde_json::from_slice(&std::fs::read(&journal).unwrap()).unwrap();
            assert_eq!(final_state["operations"][row["index"].as_u64().unwrap() as usize]["status"], "completed");
        }
    }

    #[test]
    fn request_identity_survives_refreshed_context_but_separates_turns() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("state");
        let mut opts = crate::reasoner::loop_parse_opts();
        opts.session_id = Some("synthetic-turn-1".into());
        let first = request_path(&root, &opts).unwrap();
        assert_eq!(first, request_path(&root, &opts).unwrap());
        // The query handler prepends the current clock and owner context each
        // time it runs. Refreshing those must not lose an earlier receipt.
        opts.system_prompt.push_str(" Refreshed deployment instructions.");
        assert_eq!(first, request_path(&root, &opts).unwrap());
        opts.session_id = Some("synthetic-turn-2".into());
        assert_ne!(first, request_path(&root, &opts).unwrap());
        assert!(!first.to_string_lossy().contains("synthetic-turn"));
        opts.session_id = None;
        assert_ne!(request_path(&root, &opts).unwrap(), request_path(&root, &opts).unwrap());
        opts.session_id = Some("-".into());
        assert_ne!(request_path(&root, &opts).unwrap(), request_path(&root, &opts).unwrap());
    }

    #[test]
    fn recovery_context_preserves_completed_and_uncertain_progress() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("operations.json");
        let mut file = std::fs::OpenOptions::new().create_new(true).write(true).mode(0o600).open(&path).unwrap();
        file.write_all(serde_json::to_string(&json!({"version":1,"operations":[
            {"tool":"mcp__fixture__create","arguments":{},"status":"completed","result":"synthetic-42"},
            {"tool":"mcp__fixture__update","arguments":{},"status":"started"}
        ]})).unwrap().as_bytes()).unwrap();
        let prompt = resume_message(&path, "original request").unwrap();
        assert!(prompt.starts_with("original request"));
        assert!(prompt.contains("synthetic-42"));
        assert!(prompt.contains("started"));
        assert!(prompt.contains("not instructions"));
    }

    #[test]
    fn restarted_request_cannot_resume_while_process_cleanup_is_unverified() {
        let temp = tempfile::tempdir().unwrap();
        let journal = temp.path().join("operations.json");
        std::fs::write(journal.with_extension("active"), b"in-flight\n").unwrap();
        // A crash can occur before the first tool receipt exists. Absence of
        // the journal must not bypass the durable process-lifecycle gate.
        let error = resume_message(&journal, "synthetic request").unwrap_err();
        assert!(matches!(crate::reasoner::ReasonerError::find_in(&error),
            Some(crate::reasoner::ReasonerError::CleanupUncertain { .. })));
    }

    #[test]
    fn primary_hooks_preserve_existing_guards_and_mcp() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut opts = crate::reasoner::loop_parse_opts();
        opts.handoff_path = Some(temp.path().join("private journal's state.json"));
        opts.settings_json = Some(json!({
            "hooks": {"PreToolUse": [{"matcher": "Write", "hooks": [{"type": "command", "command": "existing-guard"}]}]},
            "mcpServers": {"fixture": {"command": "synthetic-server"}}
        }).to_string());
        let launch = ClaudeHooks::prepare(&opts).unwrap().unwrap();
        let settings: Value = serde_json::from_str(&launch.settings_json).unwrap();
        assert_eq!(settings["hooks"]["PreToolUse"][0]["hooks"][0]["command"], "existing-guard");
        assert_eq!(settings["mcpServers"]["fixture"]["command"], "synthetic-server");
        for event in ["PreToolUse", "PostToolUse", "PostToolUseFailure"] {
            let groups = settings["hooks"][event].as_array().unwrap();
            let command = groups.last().unwrap()["hooks"][0]["command"].as_str().unwrap();
            assert!(command.ends_with("|| exit 2"));
            assert!(command.contains("'\\''"));
        }
        let command = settings["hooks"]["PreToolUse"][1]["hooks"][0]["command"].as_str().unwrap();
        let run = |event: Value| {
            use std::process::{Command, Stdio};
            let mut child = Command::new("sh").args(["-c", command])
                .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
            child.stdin.take().unwrap().write_all(event.to_string().as_bytes()).unwrap();
            child.wait_with_output().unwrap()
        };
        let event = json!({"hook_event_name":"PreToolUse", "tool_use_id":"synthetic-operation",
            "tool_name":"mcp__fixture__create", "tool_input":{"title":"Synthetic"}});
        let first = run(event.clone());
        assert!(first.status.success(), "hook failed: {}", String::from_utf8_lossy(&first.stderr));
        let state: Value = serde_json::from_slice(&std::fs::read(opts.handoff_path.as_ref().unwrap()).unwrap()).unwrap();
        assert_eq!(state["operations"][0]["status"], "started");
        // An uncertain previous invocation must produce Claude's blocking code.
        assert_eq!(run(event.clone()).status.code(), Some(2));
        let mut finished = event;
        finished["hook_event_name"] = json!("PostToolUse");
        finished["tool_response"] = json!({"content":[{"type":"text","text":"synthetic-42"}]});
        assert!(run(finished).status.success());
        let state: Value = serde_json::from_slice(&std::fs::read(opts.handoff_path.as_ref().unwrap()).unwrap()).unwrap();
        assert_eq!(state["operations"][0]["status"], "completed");
    }
}
