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
    for path in [&home, &wiki, &codex_home] {
        std::fs::create_dir_all(path).unwrap();
    }
    std::fs::write(codex_home.join("auth.json"), "{}").unwrap();
    std::fs::write(wiki.join("probe.txt"), NONCE).unwrap();
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
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let fakes = repo.join("scripts/reasoner-fault-injection");
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
    assert_eq!(rows.len(), 3, "{audit}");
    for (index, profile) in ["qwen", "glm", "codex"].iter().enumerate() {
        assert_eq!(rows[index]["provider"], *profile);
        assert_eq!(rows[index]["tool"], "Read");
        assert_eq!(
            rows[index]["session_id"],
            format!("{CHANNEL}:{}", index + 1)
        );
        assert!(rows[index]["stdout_truncated"]
            .as_str()
            .unwrap()
            .contains(NONCE));
        assert!(rows[index]["stderr_truncated"].is_null());
    }
    assert_eq!(
        std::fs::read_to_string(home.join(".fake-cli/codex.count"))
            .unwrap()
            .trim(),
        "3"
    );
    assert!(!home.join(".fake-cli/claude.count").exists());
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
        repo_root: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap(),
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
        let answer = handler
            .answer(
                &augmentagent_approval_discord::AuditCtx {
                    session_id: format!("{CHANNEL}:{}", index + 1),
                    http: None,
                    channel_id: Some(serenity::model::id::ChannelId::new(CHANNEL)),
                    owner_authorized: true,
                },
                &prompt,
            )
            .await
            .unwrap();
        assert_eq!(answer.trim(), NONCE);
        history.push_str(&format!("user: {question}\nassistant: {answer}\n"));
    }
}
