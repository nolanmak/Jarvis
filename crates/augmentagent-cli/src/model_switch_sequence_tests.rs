//! Exercise the Discord query handler's persisted `/model` selection across
//! three turns without Discord credentials or paid model calls.
use super::*;
#[cfg(target_os = "linux")]
use std::process::Command;

const CHANNEL: u64 = 987654321;
const NONCE: &str = "TOOL_PROBE_01234567-89ab-cdef-0123-456789abcdef";

#[cfg(target_os = "linux")]
#[test]
fn discord_model_switch_sequence_uses_one_audited_harness() {
    let scratch = tempfile::tempdir().expect("scratch directory");
    let home = scratch.path().join("home");
    let wiki = scratch.path().join("wiki");
    let codex_home = scratch.path().join("codex-home");
    let repo = scratch.path().join("repo");
    let scripts = repo.join("scripts");
    let release = repo.join("target/release");
    for path in [&home, &wiki, &codex_home, &scripts, &release] {
        std::fs::create_dir_all(path).unwrap();
    }
    let source_repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    std::fs::copy(
        source_repo.join("scripts/aa-wiki-scope-guard.sh"),
        scripts.join("aa-wiki-scope-guard.sh"),
    )
    .unwrap();
    let memory = release.join("augmentagent-mcp-memory");
    std::fs::write(
        &memory,
        r#"#!/usr/bin/env python3
import json, sys
names = ('search_conversation_history', 'read_conversation_thread',
         'search_messages', 'conversation_stats', 'memory_search', 'memory_recent', 'switch_model')
for line in sys.stdin:
    request = json.loads(line)
    method = request.get('method')
    if method == 'initialize':
        result = {'protocolVersion': '2024-11-05', 'capabilities': {'tools': {}},
                  'serverInfo': {'name': 'synthetic-memory', 'version': '1'}}
    elif method == 'tools/list':
        result = {'tools': [{'name': name, 'description': 'Synthetic memory',
                            'inputSchema': {'type': 'object', 'properties': {}}}
                           for name in names]}
    elif method == 'tools/call':
        result = {'content': [{'type': 'text', 'text': 'MEMORY_FIXTURE_OK'}]}
    else:
        result = {}
    if 'id' in request:
        print(json.dumps({'jsonrpc': '2.0', 'id': request['id'], 'result': result}), flush=True)
"#,
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&memory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let finance = release.join("augmentagent");
    std::fs::write(
        &finance,
        "#!/bin/sh\nif [ \"$1\" = model-tool ]; then exec \"$(dirname \"$0\")/augmentagent-mcp-memory\"; fi\n[ \"$#\" -eq 2 ] && [ \"$1\" = finance ] && [ \"$2\" = status ] || exit 64\nprintf 'SHELL_FIXTURE_OK\\n'\n",
    )
    .unwrap();
    std::fs::set_permissions(&finance, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(codex_home.join("auth.json"), "{}").unwrap();
    let router = scratch.path().join("model-router.json");
    std::fs::write(
        &router,
        serde_json::json!({
            "version": 1,
            "mode": "direct",
            "base_url": "http://127.0.0.1:20128/v1",
            "api_key": "synthetic-router-key",
            "models": {
                "claude": {"quality": "cc/claude-test", "fast": "cc/claude-fast"},
                "codex": {"quality": "cx/codex-test", "fast": "cx/codex-fast"}
            }
        })
        .to_string(),
    )
    .unwrap();
    let fakes = source_repo.join("scripts/reasoner-fault-injection");
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "model_switch_sequence_tests::discord_model_switch_sequence_child",
            "--nocapture",
        ])
        .current_dir(scratch.path())
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("XDG_STATE_HOME", scratch.path().join("state"))
        .env("AUGMENTAGENT_CODEX_HOME", &codex_home)
        .env("AUGMENTAGENT_REASONER_CHAIN", "claude,codex")
        .env(
            "AUGMENTAGENT_COOLDOWN_FILE",
            scratch.path().join("cooldowns.json"),
        )
        .env("AUGMENTAGENT_MODEL_ROUTER_CONFIG", &router)
        .env(
            "AUGMENTAGENT_MODEL_SELECTION_CONFIG",
            scratch.path().join("selection.json"),
        )
        .env("AUGMENTAGENT_MODEL_QWEN_ENABLED", "1")
        .env("AUGMENTAGENT_MODEL_GLM_ENABLED", "1")
        .env(
            "AUGMENTAGENT_TOOL_AUDIT_LOG",
            scratch.path().join("tool-audit.jsonl"),
        )
        .env("CLAUDE_CLI", fakes.join("fake-claude-ok.sh"))
        .env("CODEX_CLI", fakes.join("fake-codex-switch-probe.sh"))
        .env("MODEL_SWITCH_TEST_ROOT", scratch.path())
        .output()
        .expect("child test process");
    assert!(
        output.status.success(),
        "child stdout:\n{}\nchild stderr:\n{}\nprobe stderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        std::fs::read_to_string(home.join(".fake-cli/switch-error.log")).unwrap_or_default()
    );

    let usage =
        std::fs::read_to_string(scratch.path().join("state/augmentagent/token-usage.jsonl"))
            .unwrap();
    let usage_rows: Vec<serde_json::Value> = usage
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(usage_rows.len(), 3, "{usage}");
    for (row, profile) in usage_rows.iter().zip(["qwen", "glm", "codex"]) {
        assert_eq!(row["provider"], profile);
    }
    assert_eq!(usage_rows[0]["model"], "runpod/qwen38-27b");
    assert_eq!(usage_rows[1]["model"], "runpod/glm-5.3-flash");
    let audit = std::fs::read_to_string(scratch.path().join("tool-audit.jsonl")).unwrap();
    let rows: Vec<serde_json::Value> = audit
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(rows.len(), 13, "{audit}");
    for row in &rows {
        let turn: usize = row["session_id"]
            .as_str()
            .unwrap()
            .rsplit(':')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(row["model"], usage_rows[turn - 1]["model"], "{row}");
    }
    assert_eq!(rows[0]["provider"], "qwen");
    assert_eq!(rows[0]["tool"], "Write");
    assert_eq!(rows[0]["session_id"], format!("{CHANNEL}:1"));
    assert!(rows[0]["stderr_truncated"].is_null());
    for (index, profile) in ["qwen", "glm", "codex"].iter().enumerate() {
        let read_index = if index == 0 { 1 } else { index * 4 + 1 };
        let row = &rows[read_index];
        assert_eq!(row["provider"], *profile);
        assert_eq!(row["tool"], "Read");
        assert_eq!(row["session_id"], format!("{CHANNEL}:{}", index + 1));
        assert!(row["stdout_truncated"].as_str().unwrap().contains(NONCE));
        assert!(row["stderr_truncated"].is_null());
        let memory = &rows[read_index + 1];
        assert_eq!(memory["provider"], *profile);
        assert_eq!(memory["tool"], "mcp__memory__memory_recent");
        assert_eq!(memory["session_id"], format!("{CHANNEL}:{}", index + 1));
        assert!(memory["stdout_truncated"]
            .as_str()
            .unwrap()
            .contains("MEMORY_FIXTURE_OK"));
        assert!(memory["stderr_truncated"].is_null());
        let edit = &rows[read_index + 2];
        assert_eq!(edit["provider"], *profile);
        assert_eq!(edit["tool"], "Edit");
        assert_eq!(edit["session_id"], format!("{CHANNEL}:{}", index + 1));
        assert!(edit["stderr_truncated"].is_null());
        let shell = &rows[read_index + 3];
        assert_eq!(shell["provider"], *profile);
        assert_eq!(shell["tool"], "Bash");
        assert_eq!(shell["session_id"], format!("{CHANNEL}:{}", index + 1));
        assert_eq!(shell["exit_code"], 0);
        assert_eq!(shell["runner"], "host");
        assert!(shell["stdout_truncated"]
            .as_str()
            .unwrap()
            .contains("SHELL_FIXTURE_OK"));
    }
    assert_eq!(
        std::fs::read_to_string(wiki.join("probe.txt")).unwrap(),
        format!("{NONCE}|qwen|glm|codex")
    );
    assert_eq!(
        std::fs::read_to_string(home.join(".fake-cli/codex.count"))
            .unwrap()
            .trim(),
        "3"
    );
    assert!(home.join(".fake-cli/switch-entered").exists());
    assert!(home.join(".fake-cli/switch-release").exists());
    assert_eq!(std::fs::read_to_string(home.join(".fake-cli/claude.count")).unwrap().trim(), "1");
}

#[tokio::test]
async fn discord_model_switch_sequence_child() {
    let Ok(root) = std::env::var("MODEL_SWITCH_TEST_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    let handler = WikiQuerier {
        reasoner: build_reasoner(),
        wiki_root: root.join("wiki"),
        repo_root: root.join("repo"),
    };
    let file = root.join("wiki/probe.txt");
    let mut history = String::new();
    for (index, profile) in ["qwen", "glm", "codex"].iter().enumerate() {
        let reply = handler
            .model_command(CHANNEL, &format!("/model set {profile}"))
            .await
            .unwrap();
        assert!(reply.contains(profile), "{reply}");
        assert_eq!(
            handler.selected_model(CHANNEL).await.unwrap().as_deref(),
            Some(*profile)
        );
        assert_eq!(handler.selected_model(CHANNEL + 1).await.unwrap(), None);
        let question = format!("TOOL_PROBE_FILE: {}", file.display());
        let prompt = if history.is_empty() {
            question.clone()
        } else {
            format!("<conversation_history>\n{history}</conversation_history>\n\nuser's current message:\n{question}")
        };
        let ctx = augmentagent_approval_discord::AuditCtx {
            session_id: format!("{CHANNEL}:{}", index + 1),
            http: None,
            channel_id: Some(serenity::model::id::ChannelId::new(CHANNEL)),
            owner_authorized: true,
        };
        let answer = if index == 1 {
            let entered = root.join("home/.fake-cli/switch-entered");
            let release = root.join("home/.fake-cli/switch-release");
            let (answer, ()) = tokio::join!(handler.answer(&ctx, &prompt), async {
                tokio::time::timeout(std::time::Duration::from_secs(30), async {
                    while !entered.exists() {
                        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                    }
                })
                .await
                .expect("GLM turn started before switch");
                let switched = handler
                    .model_command(CHANNEL, "/model set codex")
                    .await
                    .unwrap();
                assert!(switched.contains("codex"), "{switched}");
                assert_eq!(
                    handler.selected_model(CHANNEL).await.unwrap().as_deref(),
                    Some("codex")
                );
                std::fs::write(release, b"continue").unwrap();
            });
            answer.unwrap()
        } else {
            handler.answer(&ctx, &prompt).await.unwrap()
        };
        assert_eq!(answer.trim(), NONCE);
        history.push_str(&format!("user: {question}\nassistant: {answer}\n"));
    }
    let reply = handler.model_command(CHANNEL, "/model set claude").await.unwrap();
    assert!(reply.contains("set to claude"), "{reply}");
    assert_eq!(handler.selected_model(CHANNEL).await.unwrap().as_deref(), Some("claude"));
    let ctx = augmentagent_approval_discord::AuditCtx {
        session_id: format!("{CHANNEL}:4"),
        http: None,
        channel_id: Some(serenity::model::id::ChannelId::new(CHANNEL)),
        owner_authorized: true,
    };
    let answer = handler.answer(&ctx, "Reply with the diagnostic response").await.unwrap();
    assert_eq!(answer.trim(), "PONG-FROM-FAKE-CLAUDE");
    handler.model_command(CHANNEL, "/model reset").await.unwrap();
    assert_eq!(handler.selected_model(CHANNEL).await.unwrap(), None);

}
