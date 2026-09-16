//! Codex CLI reasoner adapter with a constrained Jarvis tool bridge (#1019).
//!
//! Each call uses an empty native workspace, minimal native filesystem access,
//! and a required MCP bridge compiled into the adapter. The bridge receives
//! the preset's scope, tools, guards and integration configuration privately.
//! Model-generated commands are parsed into argv; general commands use the
//! bridge's kernel sandbox and trusted service verbs retain approval checks.
//!
//! Native project instructions, user config, plugins and shell tools are
//! excluded. Images supplied by the caller still use native `-i` input.
//! Provider authentication uses the existing CODEX_API_KEY/keyring or persistent
//! CODEX_HOME login. Integration credentials do not reach the native tool env.
//!
//! The adapter captures final or all assistant blocks and normalizes bridge
//! tool events into the common audit log. Routing eligibility remains a separate
//! capability gate; adapter smoke tests alone do not establish full parity.

use std::process::Stdio;

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tracing::{debug, warn};

use crate::providers::{model_for, tier_of, ProviderKind};
use crate::reasoner::{
    caller_tag, parse_reset_hint, reasoner_timeout, Reasoner, ReasonerError, ReasonerOpts,
};

/// Codex binary override (`CODEX_CLI`, mirroring `CLAUDE_CLI`) — also how
/// the fault-injection test rig (#666) points the adapter at a stub script.
pub fn codex_bin() -> String {
    std::env::var("CODEX_CLI").unwrap_or_else(|_| "codex".into())
}

/// Resolved CODEX_HOME: `AUGMENTAGENT_CODEX_HOME` override, else `~/.codex`
/// (where `codex login` writes `auth.json`).
pub fn codex_home() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("AUGMENTAGENT_CODEX_HOME") {
        if !p.trim().is_empty() {
            return std::path::PathBuf::from(p);
        }
    }
    std::env::var_os("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".codex"))
        .unwrap_or_else(|| std::path::PathBuf::from(".codex"))
}

/// Is there any way for the codex adapter to authenticate? Either an API key
/// (keyring/env) or a ChatGPT-plan `auth.json` under the resolved home.
pub fn codex_auth_available() -> bool {
    crate::secret_loader::load_provider_key("CODEX_API_KEY").is_some()
        || codex_home().join("auth.json").is_file()
}

pub struct CodexCliReasoner {
    bin: String,
    /// #898 — shared cap on concurrent CLI children.
    gate: std::sync::Arc<crate::cli_gate::CliGate>,
}

async fn record_tool_item(opts: &ReasonerOpts, item: &serde_json::Value) {
    use crate::tool_audit::{build_audit_record, is_high_risk};
    if item.get("type").and_then(|v| v.as_str()) != Some("mcp_tool_call") { return; }
    let server = item.get("server").and_then(|v| v.as_str()).unwrap_or("unknown");
    let leaf = item.get("tool").and_then(|v| v.as_str()).unwrap_or("unknown");
    let tool = if server == "jarvis" { leaf.to_string() } else { format!("mcp__{server}__{leaf}") };
    let args = item.get("arguments").cloned().unwrap_or(serde_json::Value::Null);
    let result = item.get("result");
    let content = result.and_then(|r| r.get("content")).and_then(|r| r.as_array())
        .map(|items| items.iter().filter_map(|r| r.get("text").and_then(|t| t.as_str())).collect::<Vec<_>>().join("\n"))
        .unwrap_or_else(|| item.get("error").filter(|e| !e.is_null()).map(ToString::to_string).unwrap_or_default());
    let failed = item.get("status").and_then(|s| s.as_str()) == Some("failed")
        || result.is_some_and(|r| r.get("isError").or_else(|| r.get("is_error")).and_then(|b| b.as_bool()).unwrap_or(false));
    let session = opts.session_id.as_deref().unwrap_or("-");
    let mut record = build_audit_record(chrono::Utc::now().to_rfc3339(), session.into(), tool.clone(), args, &content, failed);
    record.provider = Some("codex".into());
    if tool == "Bash" {
        record.exit_code = serde_json::from_str::<serde_json::Value>(&content).ok()
            .and_then(|v| v.get("exit_code").and_then(|c| c.as_i64())).and_then(|c| i32::try_from(c).ok());
    }
    if let Some(logger) = &opts.audit_logger { logger.record(&record).await; }
    if is_high_risk(&tool) {
        if let Some(notifier) = &opts.audit_notifier { notifier.notify(session, &record).await; }
    }
}

impl CodexCliReasoner {
    pub fn openai() -> Self {
        Self {
            bin: codex_bin(),
            gate: crate::cli_gate::CliGate::global(),
        }
    }

    fn provider_name(&self) -> &'static str {
        ProviderKind::Codex.name()
    }

    async fn call_capture(
        &self,
        opts: &ReasonerOpts,
        user_message: &str,
        all_blocks: bool,
    ) -> anyhow::Result<String> {
        let provider = self.provider_name();
        let dur = reasoner_timeout();
        // #898 — CLI slot before the watchdog starts; #954 — the wait carries
        // the same budget, so it can never outlive the call it precedes.
        let caller = caller_tag(opts);
        let acquire = self.gate.acquire_timed("codex", &caller, dur);
        let _permit = acquire.await.map_err(ReasonerError::from)?;
        match tokio::time::timeout(dur, self.call_once(opts, user_message, all_blocks)).await {
            // Post-classify any untyped failure (stdin EPIPE, read/wait IO)
            // as provider-side Unavailable (#655 review) — an untyped error
            // would abort the whole chain instead of failing over.
            Ok(Err(e)) if ReasonerError::find_in(&e).is_none() => {
                Err(crate::reasoner::classify_other(provider, e))
            }
            Ok(r) => r,
            Err(_) => {
                warn!(
                    "{provider} call exceeded the {}s watchdog; child killed",
                    dur.as_secs()
                );
                Err(anyhow::Error::new(ReasonerError::Timeout {
                    provider: provider.into(),
                    secs: dur.as_secs(),
                }))
            }
        }
    }

    async fn call_once(
        &self,
        opts: &ReasonerOpts,
        user_message: &str,
        all_blocks: bool,
    ) -> anyhow::Result<String> {
        let provider = self.provider_name();
        let model = model_for(ProviderKind::Codex, tier_of(opts));

        // `IMAGE:` markers → native `-i` attachments (see crate::images).
        // The marker lines are stripped from the stdin prompt; codex embeds
        // the images in the request itself, so an image turn survives a
        // claude→codex failover instead of arriving as a dead file path.
        let extracted = crate::images::extract_image_markers(user_message);
        let (user_message, image_paths) = if extracted.images.is_empty() {
            (user_message.to_string(), Vec::new())
        } else {
            (
                format!(
                    "{}\n\n[{} image attachment(s) are embedded in this prompt]",
                    extracted.text,
                    extracted.images.len()
                ),
                extracted.images,
            )
        };
        let user_message = user_message.as_str();

        // System prompt travels via a temp file (`model_instructions_file`
        // has no inline-string form). TempDir must outlive the child.
        let tmp = tempfile::tempdir().map_err(|e| {
            anyhow::Error::new(ReasonerError::Local {
                message: format!("{provider}: tempdir for instructions failed: {e}"),
            })
        })?;
        let effective_system = if opts.system_prompt.trim().is_empty() {
            "You are a concise assistant."
        } else {
            &opts.system_prompt
        };
        let bridge = crate::codex_tools::BridgeLaunch::prepare(opts, tmp.path()).map_err(|error| {
            anyhow::Error::new(ReasonerError::Local {
                message: format!("codex tool policy is not ready: {error}"),
            })
        })?;
        let effective_system = format!("{effective_system}\n\nJarvis tool transport: use the jarvis MCP tools for the \
            declared file, command and integration operations. The tool workspace is {}. \
            Relative tool paths resolve there. Commands accept one executable and its arguments; \
            split compound shell operations into separate calls. Preserve ATTACH output markers \
            verbatim when delivering original files. Use native web tools for declared web operations.",
            opts.cwd.as_ref().or(opts.add_dirs.first()).map(|p| p.display().to_string()).unwrap_or_default());
        let instructions = tmp.path().join("instructions.md");
        std::fs::write(&instructions, effective_system).map_err(|e| {
            anyhow::Error::new(ReasonerError::Local {
                message: format!("{provider}: writing instructions file failed: {e}"),
            })
        })?;

        let mut args: Vec<String> = vec![
            "exec".into(),
            "--json".into(),
            "--skip-git-repo-check".into(),
            "--ignore-user-config".into(),
            "--ignore-rules".into(),
            "--ephemeral".into(),
            "--strict-config".into(),
            "-c".into(),
            "approval_policy=never".into(),
            // AGENTS.md is project-doc discovery, NOT user config — verified
            // live that --ignore-user-config does not stop it. Zero the
            // budget so repo instructions can't inject into background calls.
            "-c".into(),
            "project_doc_max_bytes=0".into(),
            "-c".into(),
            format!("model_instructions_file={}", serde_json::to_string(&instructions)?),
            "-m".into(),
            model.clone(),
        ];
        for img in &image_paths {
            args.push("-i".into());
            args.push(img.to_string_lossy().into_owned());
        }
        for config in &bridge.config_overrides {
            args.push("-c".into());
            args.push(config.clone());
        }
        args.push("-C".into());
        args.push(bridge.native_cwd.to_string_lossy().into_owned());
        // "-" = read the prompt from stdin, mirroring the claude spawn shape.
        args.push("-".into());

        let mut cmd = Command::new(&self.bin);
        cmd.args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        // Always a clean env (the #128 posture): OS essentials + CODEX_HOME
        // + exactly the secrets this backend needs, JIT-loaded. The daemon's
        // .env keys never leak into a codex spawn.
        cmd.env_clear();
        for var in ["HOME", "PATH", "USER", "LOGNAME", "TERM", "LANG", "SHELL"] {
            if let Ok(v) = std::env::var(var) {
                cmd.env(var, v);
            }
        }
        cmd.env("CODEX_HOME", codex_home());
        if let Some(key) = crate::secret_loader::load_provider_key("CODEX_API_KEY") {
            cmd.env("CODEX_API_KEY", key);
        }
        // else: auth.json under CODEX_HOME carries ChatGPT-plan auth.
        //
        // Integration environment is carried in the owner-private broker
        // policy, never exposed to native tools or placed on argv.


        let (mut child, process_group) = crate::process_tree::spawn(&mut cmd).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                anyhow::Error::new(ReasonerError::Local {
                    message: format!("{provider}: binary {:?} not found on PATH", self.bin),
                })
            } else {
                anyhow::Error::new(ReasonerError::Unavailable {
                    provider: provider.into(),
                    message: format!("spawn failed: {e}"),
                })
            }
        })?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(user_message.as_bytes()).await?;
            stdin.shutdown().await?;
        }

        // Drain stderr CONCURRENTLY (#655 review): a child writing >64KB of
        // progress/log output to a full stderr pipe would block, stdout
        // would never reach EOF, and the call would sit until the watchdog
        // instead of failing over in seconds.
        let stderr_task = child.stderr.take().map(|mut err| {
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut buf = String::new();
                let _ = err.read_to_string(&mut buf).await;
                buf
            })
        });
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("{provider} stdout missing"))?;
        let mut lines = BufReader::new(stdout).lines();

        // Final assistant text = agent_message items, in order. Failure text
        // = turn.failed / stream error events.
        let mut messages: Vec<String> = Vec::new();
        let mut failure: Option<String> = None;
        while let Some(line) = lines.next_line().await? {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                debug!("{provider} jsonl parse skip: {line}");
                continue;
            };
            if v.get("type").and_then(|t| t.as_str()) == Some("item.completed") {
                if let Some(item) = v.get("item") {
                    record_tool_item(opts, item).await;
                }
            }
            match v.get("type").and_then(|t| t.as_str()) {
                Some("item.completed") => {
                    let item = v.get("item");
                    if item.and_then(|i| i.get("type")).and_then(|t| t.as_str())
                        == Some("agent_message")
                    {
                        if let Some(text) =
                            item.and_then(|i| i.get("text")).and_then(|t| t.as_str())
                        {
                            if !text.trim().is_empty() {
                                messages.push(text.to_string());
                            }
                        }
                    }
                }
                Some("turn.failed") => {
                    failure = v
                        .get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(|m| m.as_str())
                        .map(str::to_string)
                        .or(Some("turn.failed with no message".into()));
                }
                Some("error") => {
                    // Stream-level errors include transient "Reconnecting…"
                    // notices; only keep as failure if nothing succeeds.
                    if failure.is_none() {
                        failure = v
                            .get("message")
                            .and_then(|m| m.as_str())
                            .map(str::to_string);
                    }
                }
                _ => {}
            }
        }

        let status = child.wait().await?;
        drop(process_group);
        let stderr_buf = match stderr_task {
            Some(t) => t.await.unwrap_or_default(),
            None => String::new(),
        };

        let final_text = if all_blocks {
            messages.join("\n\n")
        } else {
            messages.last().cloned().unwrap_or_default()
        };

        if status.success() && !final_text.is_empty() {
            // Defensive: some limiter builds have surfaced the refusal as
            // ordinary text (the claude failure shape). Catch it here too.
            if crate::reasoner::is_rate_limited(&final_text) {
                return Err(rate_limit_err(provider, final_text));
            }
            return Ok(final_text);
        }

        let detail = failure
            .or_else(|| {
                stderr_buf
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| format!("{provider} exited {status:?} with no output"));
        warn!("{provider} exec failed: {detail}");
        if looks_rate_limited(&detail) {
            return Err(rate_limit_err(provider, detail));
        }
        Err(anyhow::Error::new(ReasonerError::Unavailable {
            provider: provider.into(),
            message: detail.chars().take(300).collect(),
        }))
    }
}

/// Quota-shaped failure text across both backends: ChatGPT-plan usage-limit
/// wording, platform 429/insufficient_quota, and Cerebras' 429 RateLimitError
/// / 402 credits-exhausted (a spend wall latches exactly like a rate wall).
fn looks_rate_limited(detail: &str) -> bool {
    let d = detail.to_ascii_lowercase();
    // Status codes match as standalone digit tokens only (#655 review):
    // bare substring "429" fired inside unrelated numbers like request ids.
    let has_code = |code: &str| {
        d.split(|c: char| !c.is_ascii_digit())
            .any(|tok| tok == code)
    };
    d.contains("usage limit")
        || d.contains("rate limit")
        || d.contains("rate_limit")
        || d.contains("insufficient_quota")
        || d.contains("resource_exhausted")
        || d.contains("payment required")
        || d.contains("quota exceeded")
        || d.contains("quota exhausted")
        || has_code("429")
        || has_code("402")
}

fn rate_limit_err(provider: &str, message: String) -> anyhow::Error {
    let reset_at = parse_reset_hint(&message);
    anyhow::Error::new(ReasonerError::RateLimited {
        provider: provider.into(),
        message,
        reset_at,
    })
}

#[async_trait]
impl Reasoner for CodexCliReasoner {
    async fn call(&self, opts: &ReasonerOpts, user_message: &str) -> anyhow::Result<String> {
        self.call_capture(opts, user_message, false).await
    }

    async fn call_transcript(
        &self,
        opts: &ReasonerOpts,
        user_message: &str,
    ) -> anyhow::Result<String> {
        self.call_capture(opts, user_message, true).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ModelTier;
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt;

    fn stub(dir: &tempfile::TempDir, name: &str, body: &str) -> String {
        let path = dir.path().join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "#!/usr/bin/env bash").unwrap();
        f.write_all(body.as_bytes()).unwrap();
        drop(f);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_string_lossy().into_owned()
    }

    fn opts() -> ReasonerOpts {
        ReasonerOpts {
            system_prompt: "You classify emails.".into(),
            model: Some("claude-opus-4-8".into()),
            allowed_tools: vec![],
            add_dirs: vec![],
            permission_mode: "default".into(),
            cwd: None,
            env: vec![],
            settings_json: None,
            restrict_env: false,
            audit_logger: None,
            audit_notifier: None,
            session_id: None,
            handoff_path: None,
        }
    }

    #[tokio::test]
    async fn parses_final_agent_message_from_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let bin = stub(
            &dir,
            "fake-codex",
            r#"
cat >/dev/null
echo '{"type":"thread.started","thread_id":"t1"}'
echo '{"type":"item.completed","item":{"type":"reasoning","text":"thinking"}}'
echo '{"type":"item.completed","item":{"type":"agent_message","text":"scratch note"}}'
echo '{"type":"item.completed","item":{"type":"agent_message","text":"{\"decision\":\"reply\"}"}}'
echo '{"type":"turn.completed","usage":{"input_tokens":10}}'
"#,
        );
        let r = CodexCliReasoner {
            bin,
            gate: crate::cli_gate::CliGate::global(),
        };
        let got = r.call(&opts(), "classify this").await.unwrap();
        assert_eq!(got, "{\"decision\":\"reply\"}", "LastBlock keeps the final message");
        let all = r.call_transcript(&opts(), "classify this").await.unwrap();
        assert!(all.contains("scratch note") && all.contains("decision"));
    }

    #[tokio::test]
    async fn tool_using_spawn_has_a_required_scoped_bridge_and_no_native_shell() {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("argv.txt");
        let bin = stub(&dir, "fake-codex", &format!(
            "cat >/dev/null\nprintf '%s\\n' \"$@\" >{}\necho '{{\"type\":\"item.completed\",\"item\":{{\"type\":\"agent_message\",\"text\":\"ok\"}}}}'\n",
            record.display()
        ));
        let mut options = crate::reasoner::resume_opts(dir.path().into());
        options.env.push(("SYNTHETIC_TOKEN".into(), "private-fixture-only".into()));
        let reasoner = CodexCliReasoner { bin, gate: crate::cli_gate::CliGate::global() };
        reasoner.call(&options, "Read a synthetic note").await.unwrap();
        let args = std::fs::read_to_string(record).unwrap();
        assert!(args.contains("mcp_servers.jarvis="));
        assert!(args.contains("required=true"));
        assert!(args.contains("features.shell_tool=false"));
        assert!(args.contains("default_permissions=jarvis_bridge"));
        assert!(!args.contains("private-fixture-only"));
        assert!(!args.contains("danger-full-access"));
    }

    /// Live receipt, separate from deterministic adapter/stub tests.
    #[tokio::test]
    #[ignore = "requires a logged-in Codex CLI; creates synthetic files only"]
    async fn live_scoped_bridge_reads_writes_and_executes_a_command() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("source.txt"), "SYNTHETIC_SOURCE_4F2A\n").unwrap();
        let mut options = crate::reasoner::resume_opts(dir.path().into());
        options.system_prompt = "You verify a synthetic tool fixture. Follow the exact requested operations.".into();
        options.allowed_tools.push("Bash(printf *)".into());
        let audit = dir.path().join("audit.jsonl");
        options.audit_logger = Some(std::sync::Arc::new(crate::tool_audit::AuditLogger::new(audit.clone())));
        let result = CodexCliReasoner::openai().call(&options,
            "Use jarvis Read on source.txt, jarvis Write to copy its exact content INCLUDING its trailing newline into result.txt, \
             then jarvis Bash to execute printf SYNTHETIC_COMMAND_7B3C. \
             Report the actual tool outcomes and command output.").await.unwrap();
        assert_eq!(std::fs::read(dir.path().join("result.txt")).unwrap(), b"SYNTHETIC_SOURCE_4F2A\n");
        assert!(result.contains("SYNTHETIC_COMMAND_7B3C"), "{result}");
        let records = std::fs::read_to_string(audit).unwrap();
        assert!(records.lines().filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .any(|r| r["tool"] == "Bash" && r["provider"] == "codex" && r["exit_code"] == 0
                && r["stdout_truncated"].as_str().is_some_and(|s| s.contains("SYNTHETIC_COMMAND_7B3C"))));
    }

    #[tokio::test]
    async fn codex_bridge_calls_are_recorded_in_the_common_audit_log() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("audit.jsonl");
        let bin = stub(&dir, "fake-codex-audit", r#"
cat >/dev/null
echo '{"type":"item.completed","item":{"id":"synthetic-tool","type":"mcp_tool_call","server":"jarvis","tool":"Write","arguments":{"file_path":"note.md","content":"synthetic"},"result":{"content":[{"type":"text","text":"written"}]},"status":"completed"}}'
echo '{"type":"item.completed","item":{"type":"agent_message","text":"ok"}}'
"#);
        let mut options = opts();
        options.audit_logger = Some(std::sync::Arc::new(crate::tool_audit::AuditLogger::new(log.clone())));
        options.session_id = Some("synthetic-session".into());
        CodexCliReasoner { bin, gate: crate::cli_gate::CliGate::global() }
            .call(&options, "synthetic audit probe").await.unwrap();
        let row: serde_json::Value = serde_json::from_str(std::fs::read_to_string(log).unwrap().trim()).unwrap();
        assert_eq!(row["provider"], "codex");
        assert_eq!(row["tool"], "Write");
        assert_eq!(row["session_id"], "synthetic-session");
        assert_eq!(row["stdout_truncated"], "written");
    }

    #[tokio::test]
    async fn usage_limit_turn_failed_maps_to_rate_limited() {
        let dir = tempfile::tempdir().unwrap();
        let bin = stub(
            &dir,
            "fake-codex-limit",
            r#"
cat >/dev/null
echo '{"type":"turn.failed","error":{"message":"You'\''ve hit your usage limit. Try again at Aug 20th, 2026 10:27 AM."}}'
exit 1
"#,
        );
        let r = CodexCliReasoner {
            bin,
            gate: crate::cli_gate::CliGate::global(),
        };
        let err = r.call(&opts(), "hi").await.unwrap_err();
        match ReasonerError::find_in(&err) {
            Some(ReasonerError::RateLimited { provider, .. }) => assert_eq!(provider, "codex"),
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    /// #658 — the argv end of the tier map: `-m` carries exactly what
    /// `model_for` resolved, on BOTH tiers, never codex's own default.
    #[tokio::test]
    async fn spawn_pins_the_resolved_model_on_both_tiers() {
        for (preset_model, tier) in [
            ("claude-opus-4-8", ModelTier::Quality),
            ("claude-haiku-4-5-20251001", ModelTier::Fast),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let record = dir.path().join("argv.txt");
            let bin = stub(
                &dir,
                "fake-codex",
                &format!(
                    "cat >/dev/null\nprintf '%s\\n' \"$@\" >{}\necho '{{\"type\":\"item.completed\",\"item\":{{\"type\":\"agent_message\",\"text\":\"ok\"}}}}'\n",
                    record.display()
                ),
            );
            let mut o = opts();
            o.model = Some(preset_model.into());
            CodexCliReasoner {
            bin,
            gate: crate::cli_gate::CliGate::global(),
        }.call(&o, "hi").await.unwrap();
            let argv: Vec<String> = std::fs::read_to_string(&record)
                .unwrap()
                .lines()
                .map(str::to_string)
                .collect();
            let want = model_for(ProviderKind::Codex, tier);
            assert!(
                argv.windows(2).any(|w| w[0] == "-m" && w[1] == want),
                "codex must spawn with `-m {want}`, got {argv:?}"
            );
        }
    }

    /// `IMAGE:` markers become native `-i` attachments and the marker lines
    /// leave the stdin prompt (crate::images convention).
    #[tokio::test]
    async fn image_markers_translate_to_i_flags() {
        let dir = tempfile::tempdir().unwrap();
        let img = dir.path().join("aa-img-7-0.png");
        std::fs::write(&img, b"\x89PNG fake").unwrap();
        let record = dir.path().join("record.txt");
        let bin = stub(
            &dir,
            "fake-codex-img",
            &format!(
                r#"
STDIN=$(cat)
printf '%s\n' "$@" > {record}
printf 'STDIN<<%s>>\n' "$STDIN" >> {record}
echo '{{"type":"item.completed","item":{{"type":"agent_message","text":"a red square"}}}}'
"#,
                record = record.display()
            ),
        );
        let r = CodexCliReasoner {
            bin,
            gate: crate::cli_gate::CliGate::global(),
        };
        let msg = format!("what is in this image?\nIMAGE: {}", img.display());
        let got = r.call(&opts(), &msg).await.unwrap();
        assert_eq!(got, "a red square");
        let rec = std::fs::read_to_string(&record).unwrap();
        assert!(rec.contains("-i\n"), "must pass -i flag: {rec}");
        assert!(rec.contains("aa-img-7-0.png"), "image path in argv");
        assert!(
            !rec.contains(&format!("IMAGE: {}", img.display())),
            "marker line must be stripped from stdin"
        );
        assert!(rec.contains("image attachment(s) are embedded"));
    }

    #[tokio::test]
    async fn missing_binary_is_local_not_failoverable_noise() {
        let r = CodexCliReasoner {
            bin: "/nonexistent/codex-bin".into(),
            gate: crate::cli_gate::CliGate::global(),
        };
        let err = r.call(&opts(), "hi").await.unwrap_err();
        assert!(matches!(
            ReasonerError::find_in(&err),
            Some(ReasonerError::Local { .. })
        ));
    }

}
