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
//! The adapter captures final or all assistant blocks, normalizes bridge
//! tool events into the common audit log, and records each call's token
//! usage from `turn.completed` in the common usage log (#1047). Routing eligibility remains a separate
//! capability gate; adapter smoke tests alone do not establish full parity.
//!
//! Failed and empty turns are routed by the table in `crate::turn_failure`
//! (#1040): quota, transport, auth and binary failures are provider-side, and
//! so is a failure the table does not recognise on a text or read call.

use std::process::Stdio;

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tracing::{debug, warn};

use crate::providers::{model_for, tier_of, ProviderKind};
use crate::reasoner::{caller_tag, reasoner_timeout, Reasoner, ReasonerError, ReasonerOpts};
use crate::turn_failure::{turn_error, FailureClass, TurnFailure};

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
        || crate::model_router::load().ok().flatten().is_some()
}

pub struct CodexCliReasoner {
    bin: String,
    /// #898 — shared cap on concurrent CLI children.
    gate: std::sync::Arc<crate::cli_gate::CliGate>,
    /// #1047 — where this adapter's token usage goes. The process-global
    /// log in production; a private file in tests, which run concurrently.
    usage_log: std::sync::Arc<crate::token_usage::UsageLogger>,
}

/// The runner the bridge reported for a Bash call (#1041). It leads a result
/// (`{"runner": "vm", ...}`, surviving truncation) or a failure raised after
/// the process started (`[runner=host] ...`). Anything else was refused
/// before a process started.
fn bridge_runner(content: &str) -> &'static str {
    static LEADING: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let leading = LEADING.get_or_init(|| regex::Regex::new(
        r#"\A(?:\[runner=(vm|host)\] |\{"runner": "(vm|host)")"#).expect("static regex"));
    match leading.captures(content).and_then(|c| c.get(1).or_else(|| c.get(2))).map(|m| m.as_str()) {
        Some("vm") => "vm",
        Some("host") => "host",
        _ => "none",
    }
}

pub(crate) async fn record_tool_item(opts: &ReasonerOpts, item: &serde_json::Value) {
    use crate::tool_audit::{build_audit_record, is_high_risk};
    let native_web = item.get("type").and_then(|v| v.as_str()) == Some("web_search");
    if !native_web && item.get("type").and_then(|v| v.as_str()) != Some("mcp_tool_call") { return; }
    let server = item.get("server").and_then(|v| v.as_str()).unwrap_or("unknown");
    let leaf = item.get("tool").and_then(|v| v.as_str()).unwrap_or("unknown");
    // Codex reports searches and page opens under the same native event type.
    // Preserve its action verbatim; an opaque `other` is not proof of a fetch.
    let tool = if native_web { "WebSearch".into() }
        else if server == "jarvis" { leaf.to_string() } else { format!("mcp__{server}__{leaf}") };
    let args = if native_web {
        serde_json::json!({"query": item.get("query"), "action": item.get("action")})
    } else { item.get("arguments").cloned().unwrap_or(serde_json::Value::Null) };
    let result = item.get("result");
    let content = result.and_then(|r| r.get("content")).and_then(|r| r.as_array())
        .map(|items| items.iter().filter_map(|r| r.get("text").and_then(|t| t.as_str())).collect::<Vec<_>>().join("\n"))
        .unwrap_or_else(|| item.get("error").filter(|e| !e.is_null()).map(ToString::to_string).unwrap_or_default());
    let failed = item.get("status").and_then(|s| s.as_str()) == Some("failed")
        || result.is_some_and(|r| r.get("isError").or_else(|| r.get("is_error")).and_then(|b| b.as_bool()).unwrap_or(false));
    let session = opts.session_id.as_deref().unwrap_or("-");
    let mut record = build_audit_record(ProviderKind::Codex, chrono::Utc::now().to_rfc3339(), session.into(), tool.clone(), args, &content, failed);
    if tool == "Bash" {
        let outcome = serde_json::from_str::<serde_json::Value>(&content).ok();
        record.exit_code = outcome.as_ref()
            .and_then(|v| v.get("exit_code").and_then(|c| c.as_i64())).and_then(|c| i32::try_from(c).ok());
        record.runner = Some(bridge_runner(&content).to_string());
    }
    // #1047 — same sink resolution as the Claude adapter (#1004): a preset
    // that passes no logger still audits to the default log, so every
    // Codex-served tool call leaves a trail, not only `ask_opts` calls.
    if let Some(logger) = opts.audit_logger.clone().or_else(crate::reasoner::default_audit_logger) {
        logger.record(&record).await;
    }
    if is_high_risk(&tool) {
        if let Some(notifier) = &opts.audit_notifier { notifier.notify(session, &record).await; }
    }
}

impl CodexCliReasoner {
    pub fn openai() -> Self {
        Self {
            bin: codex_bin(),
            gate: crate::cli_gate::CliGate::global(),
            usage_log: crate::token_usage::UsageLogger::global(),
        }
    }

    /// Adapter bound to an explicit binary (fault-injection stubs in tests).
    #[cfg(test)]
    pub(crate) fn with_bin(bin: String) -> Self {
        Self { bin, gate: crate::cli_gate::CliGate::global(), usage_log: crate::token_usage::UsageLogger::global() }
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
        let clean = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let outcome = tokio::time::timeout(dur, self.call_once(opts, user_message, all_blocks, clean.clone())).await;
        if !clean.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(ReasonerError::CleanupUncertain { provider: provider.into() }.into());
        }
        match outcome {
            // Post-classify any untyped failure (stdin EPIPE, read/wait IO)
            // as provider-side Unavailable (#655 review) — an untyped error
            // would abort the whole chain instead of failing over. A
            // TurnFailure is untyped on purpose (#1040) and passes through.
            Ok(Err(e)) if ReasonerError::find_in(&e).is_none() && e.downcast_ref::<TurnFailure>().is_none() => {
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
        clean: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> anyhow::Result<String> {
        let provider = self.provider_name();
        let router = crate::model_router::current()?.filter(|r| r.enabled());
        let model = router.as_ref().and_then(|r| r.model(ProviderKind::Codex, tier_of(opts)))
            .unwrap_or_else(|| model_for(ProviderKind::Codex, tier_of(opts)));
        let capability = crate::providers::classify(opts);

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
        if let Some(router) = &router {
            for value in router.codex_overrides() { args.extend(["-c".into(), value]); }
        }
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
            .stderr(Stdio::piped());

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
        if let Some(router) = &router { router.configure_codex(&mut cmd); }
        // else: auth.json under CODEX_HOME carries ChatGPT-plan auth.
        //
        // Integration environment is carried in the owner-private broker
        // policy, never exposed to native tools or placed on argv.


        let (mut child, process_group) = crate::process_tree::spawn_supervised(&cmd, true, clean, opts.handoff_path.as_deref()).map_err(|e| {
            if e.kind() == std::io::ErrorKind::WouldBlock {
                anyhow::Error::new(ReasonerError::CleanupUncertain { provider: provider.into() })
            } else if e.kind() == std::io::ErrorKind::NotFound {
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

        // Final assistant text = agent_message items, in order. `turn_failed`
        // = the turn's own failure event. `stream_error` = the last
        // stream-level error notice ("Reconnecting…" included), which only
        // explains a failed exit, never a turn that went on to complete.
        // `turn_began` = any turn/item event: from then on tools may have run
        // (#1040 — it decides the fail-safe default in crate::turn_failure).
        let mut messages: Vec<String> = Vec::new();
        let mut turn_failed: Option<String> = None;
        let mut stream_error: Option<String> = None;
        let mut turn_began = false;
        // #1047 — each `turn.completed` restates the thread's running total,
        // so the last one seen is this call's usage. Keep it; never sum.
        let call_started = std::time::Instant::now();
        let mut observed_usage: Option<crate::token_usage::TokenUsage> = None;
        while let Some(line) = lines.next_line().await? {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                debug!("{provider} jsonl parse skip: {line}");
                continue;
            };
            let kind = v.get("type").and_then(|t| t.as_str());
            if kind.is_some_and(|k| k.starts_with("turn.") || k.starts_with("item.")) {
                turn_began = true;
            }
            if let Some(usage) = crate::token_usage::codex_turn_usage(&v) {
                observed_usage = Some(usage);
            }
            if kind == Some("item.completed") {
                if let Some(item) = v.get("item") {
                    record_tool_item(opts, item).await;
                }
            }
            match kind {
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
                    turn_failed = v
                        .get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(|m| m.as_str())
                        .map(str::to_string)
                        .or(Some("turn.failed with no message".into()));
                }
                Some("error") => {
                    if let Some(message) = v.get("message").and_then(|m| m.as_str()) {
                        stream_error = Some(message.to_string());
                    }
                }
                _ => {}
            }
        }

        // #1047 — record what the call cost before its outcome is judged:
        // tokens spent on a turn that ends without an answer are still spent.
        // Best effort, like the Claude path: the logger swallows IO errors.
        if let Some(usage) = observed_usage {
            self.usage_log.append(&crate::token_usage::UsageRecord::for_call(
                ProviderKind::Codex,
                model.as_str(),
                capability,
                usage,
                call_started,
            ));
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
                return Err(turn_error(provider, FailureClass::Quota, capability, final_text));
            }
            return Ok(final_text);
        }

        // #1040 — which failure this is decides latching and failover, so it
        // goes through one table (crate::turn_failure), not ad-hoc checks.
        let detail = match turn_failed {
            Some(message) => message,
            // The turn finished and said nothing. Content-level, exactly like
            // claude's EmptyOutput: untyped, so no latch and no chain advance
            // (the call's writes may already have happened). Without any turn
            // event no turn ran, so a silent exit 0 falls through to the
            // pre-turn classification below: a binary failure (#1069 M2).
            None if status.success() && turn_began => return Err(TurnFailure::empty_output(provider).into()),
            None => stream_error
                .or_else(|| {
                    stderr_buf
                        .lines()
                        .find(|l| !l.trim().is_empty())
                        .map(str::to_string)
                })
                .unwrap_or_else(|| format!("{provider} exited {status:?} with no output")),
        };
        let class = crate::turn_failure::classify(&detail, turn_began);
        if class == FailureClass::Readiness {
            // Native readiness text can carry private configuration.
            warn!("{provider} exec failed ({})", class.label());
        } else {
            warn!("{provider} exec failed ({}): {detail}", class.label());
        }
        Err(turn_error(provider, class, capability, detail))
    }
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
    async fn router_reaches_spawn_with_model_auth_and_tool_restrictions() {
        let dir = tempfile::tempdir().unwrap();
        let argv = dir.path().join("argv");
        let auth = dir.path().join("auth");
        let bin = stub(&dir, "router-codex", &format!(r#"
cat >/dev/null
printf '%s\n' "$@" >{argv}
printf '%s' "$AUGMENTAGENT_ROUTER_API_KEY" >{auth}
echo '{{"type":"item.completed","item":{{"type":"agent_message","text":"ok"}}}}'
echo '{{"type":"turn.completed","usage":{{"input_tokens":10}}}}'
"#, argv=argv.display(), auth=auth.display()));
        let config = crate::model_router::parse(&crate::model_router::tests::fixture().to_string()).unwrap();
        let reasoner = CodexCliReasoner::with_bin(bin);
        let mut options = opts(); options.restrict_env = true;
        let answer = crate::model_router::SNAPSHOT.scope(Some(config), reasoner.call(&options, "test")).await.unwrap();
        assert_eq!(answer, "ok");
        let args = std::fs::read_to_string(argv).unwrap();
        assert!(args.contains("cx/gpt-5.4"));
        assert!(args.contains("wire_api=\"responses\""));
        assert!(args.contains("--ignore-user-config") && args.contains("--strict-config"));
        assert!(args.contains("mcp_servers.jarvis"), "the constrained tool bridge must remain configured");
        assert!(!args.contains("router-secret"));
        assert_eq!(std::fs::read_to_string(auth).unwrap(), "router-secret");
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
            gate: crate::cli_gate::CliGate::global(), usage_log: crate::token_usage::UsageLogger::global(),
        };
        let got = r.call(&opts(), "classify this").await.unwrap();
        assert_eq!(got, "{\"decision\":\"reply\"}", "LastBlock keeps the final message");
        let all = r.call_transcript(&opts(), "classify this").await.unwrap();
        assert!(all.contains("scratch note") && all.contains("decision"));
    }

    /// #1047 — a codex call writes exactly one usage row: provider `codex`,
    /// the model actually passed with `-m`, the call's capability class and
    /// the right counts. `turn.completed` carries the THREAD's running total
    /// (codex exec's `usage_from_last_total`), so a second event restates
    /// the first plus more; summing them would double-count.
    #[tokio::test]
    async fn a_codex_call_records_exactly_one_usage_row() {
        let dir = tempfile::tempdir().unwrap();
        let argv = dir.path().join("argv.txt");
        let bin = stub(&dir, "fake-codex-usage", &format!(r#"
cat >/dev/null
printf '%s\n' "$@" >{argv}
echo '{{"type":"thread.started","thread_id":"t1"}}'
echo '{{"type":"turn.started"}}'
echo '{{"type":"turn.completed","usage":{{"input_tokens":1000,"cached_input_tokens":600,"cache_write_input_tokens":100,"output_tokens":50,"reasoning_output_tokens":20}}}}'
echo '{{"type":"item.completed","item":{{"type":"agent_message","text":"synthetic answer"}}}}'
echo '{{"type":"turn.completed","usage":{{"input_tokens":3000,"cached_input_tokens":2000,"cache_write_input_tokens":200,"output_tokens":120,"reasoning_output_tokens":70}}}}'
"#, argv = argv.display()));
        let log = dir.path().join("token-usage.jsonl");
        let reasoner = CodexCliReasoner {
            bin,
            gate: crate::cli_gate::CliGate::global(),
            usage_log: std::sync::Arc::new(crate::token_usage::UsageLogger::new(log.clone())),
        };
        let mut options = opts();
        options.allowed_tools = vec!["Read".into()];
        assert_eq!(reasoner.call(&options, "synthetic request").await.unwrap(), "synthetic answer");

        let body = std::fs::read_to_string(&log).expect("a codex call writes a usage row");
        let rows: Vec<crate::token_usage::UsageRecord> =
            body.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(rows.len(), 1, "one call, one row: {body}");
        let row = &rows[0];
        assert_eq!(row.provider, "codex");
        let passed: Vec<String> = std::fs::read_to_string(&argv).unwrap().lines().map(str::to_string).collect();
        let model = passed.windows(2).find(|w| w[0] == "-m").map(|w| w[1].clone()).expect("-m on argv");
        assert_eq!(row.model, model, "the model actually passed to codex");
        assert_eq!(row.class, "ReadTools");
        let u = row.usage;
        assert_eq!((u.input, u.cache_read, u.cache_creation, u.output, u.reasoning_output), (800, 2000, 200, 120, 70),
            "the last (cumulative) report, not the sum: {body}");
        assert_eq!(u.total(), 3120);
    }

    /// #1047 — tokens spent on a turn that produced no answer are still
    /// spent: the row is written before the outcome is classified. A stream
    /// with no usage report writes nothing rather than a zero row.
    #[tokio::test]
    async fn usage_is_recorded_whatever_the_outcome_and_only_when_reported() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("token-usage.jsonl");
        let adapter = |name: &str, body: &str| CodexCliReasoner {
            bin: stub(&dir, name, body),
            gate: crate::cli_gate::CliGate::global(),
            usage_log: std::sync::Arc::new(crate::token_usage::UsageLogger::new(log.clone())),
        };
        let empty = adapter("fake-codex-empty-usage", "cat >/dev/null\necho '{\"type\":\"turn.started\"}'\necho '{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":9,\"cached_input_tokens\":0,\"output_tokens\":1,\"reasoning_output_tokens\":0}}'\n");
        assert!(empty.call(&opts(), "synthetic request").await.is_err());
        let silent = adapter("fake-codex-no-usage", "cat >/dev/null\necho '{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"ok\"}}'\n");
        silent.call(&opts(), "synthetic request").await.unwrap();
        let body = std::fs::read_to_string(&log).unwrap_or_default();
        assert_eq!(body.lines().count(), 1, "{body}");
        assert!(body.contains(r#""input":9"#) && body.contains(r#""class":"TextOnly""#), "{body}");
    }

    #[tokio::test]
    async fn required_bridge_readiness_failures_are_local_and_do_not_repeat_private_details() {
        let dir = tempfile::tempdir().unwrap();
        let bin = stub(&dir, "fake-codex-readiness", r#"
cat >/dev/null
echo '{"type":"turn.failed","error":{"message":"required MCP server: JARVIS_READINESS:mcp_start PRIVATE_SYNTHETIC_CONFIGURATION"}}'
exit 1
"#);
        let reasoner = CodexCliReasoner { bin, gate: crate::cli_gate::CliGate::global(), usage_log: crate::token_usage::UsageLogger::global(), };
        let error = reasoner.call(&opts(), "Synthetic request").await.unwrap_err();
        assert!(matches!(ReasonerError::find_in(&error), Some(ReasonerError::Local { .. })), "{error}");
        assert!(error.to_string().contains("MCP"));
        assert!(!error.to_string().contains("PRIVATE_SYNTHETIC"));
    }

    #[tokio::test]
    #[ignore = "requires installed Codex; verifies failure before any model tool execution"]
    async fn live_missing_mcp_server_reports_local_readiness() {
        let fixture = tempfile::tempdir().unwrap();
        let mut options = crate::reasoner::resume_opts(fixture.path().into());
        options.allowed_tools = vec!["mcp__fixture__search".into()];
        options.settings_json = Some(serde_json::json!({"mcpServers":{"fixture":{
            "command":fixture.path().join("PRIVATE_SYNTHETIC_MISSING_BINARY"),
            "env":{"SYNTHETIC_SECRET":"PRIVATE_SYNTHETIC_TOKEN"}
        }}}).to_string());
        let error = CodexCliReasoner::openai().call(&options, "Synthetic readiness probe").await.unwrap_err();
        assert!(matches!(ReasonerError::find_in(&error), Some(ReasonerError::Local { .. })), "{error}");
        assert!(error.to_string().contains("MCP"), "{error}");
        assert!(!error.to_string().contains("PRIVATE_SYNTHETIC"), "{error}");
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
        let reasoner = CodexCliReasoner { bin, gate: crate::cli_gate::CliGate::global(), usage_log: crate::token_usage::UsageLogger::global(), };
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
    #[ignore = "requires Codex login and JARVIS_TEST_MEMORY_BIN; synthetic wiki and database only"]
    async fn live_wiki_query_profile_executes_files_and_memory_mcp() {
        let memory_bin = std::env::var_os("JARVIS_TEST_MEMORY_BIN")
            .map(std::path::PathBuf::from).expect("set JARVIS_TEST_MEMORY_BIN to the built memory server");
        assert!(memory_bin.is_absolute() && memory_bin.is_file());
        let dir = tempfile::tempdir().unwrap();
        let wiki = dir.path().join("wiki");
        std::fs::create_dir(&wiki).unwrap();
        std::fs::write(wiki.join("source.txt"), "SYNTHETIC_QUERY_83AF\n").unwrap();
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut options = crate::reasoner::ask_opts(wiki.clone(), repo);
        // Replace deployment-specific dependencies with isolated fixture state;
        // retain the production instructions, tool inventory and scope hook.
        options.add_dirs = vec![wiki.clone()];
        let database = dir.path().join("synthetic.db");
        let mut settings: serde_json::Value = serde_json::from_str(options.settings_json.as_ref().unwrap()).unwrap();
        settings["mcpServers"]["memory"]["command"] = serde_json::json!(memory_bin);
        settings["mcpServers"]["memory"]["env"]["AUGMENTAGENT_DB"] = serde_json::json!(database);
        options.settings_json = Some(settings.to_string());
        options.env.retain(|(key, _)| matches!(key.as_str(), "PATH" | "WIKI_ROOT" | "AUGMENTAGENT_REPO_ROOT"));
        options.env.push(("AUGMENTAGENT_DB".into(), database.to_string_lossy().into_owned()));
        let audit = dir.path().join("audit.jsonl");
        options.audit_logger = Some(std::sync::Arc::new(crate::tool_audit::AuditLogger::new(audit.clone())));
        let response = CodexCliReasoner::openai().call(&options,
            "Run this synthetic local verification only. Use jarvis Glob to find source.txt, \
             Grep to search for SYNTHETIC_QUERY in it, and Read to read it. Write its exact bytes \
             to result.txt, then Edit result.txt to replace QUERY with VERIFIED, preserving the newline. \
             Call the configured memory_recent MCP tool with limit 1 on the empty synthetic database. \
             Do not call any external services or shell commands. Report the actual tool results.")
            .await.unwrap();
        assert!(wiki.join("result.txt").is_file(), "query did not write the fixture: {response}; audit: {}",
            std::fs::read_to_string(&audit).unwrap_or_default());
        assert_eq!(std::fs::read(wiki.join("result.txt")).unwrap(), b"SYNTHETIC_VERIFIED_83AF\n");
        let records: Vec<serde_json::Value> = std::fs::read_to_string(audit).unwrap().lines()
            .map(|line| serde_json::from_str(line).unwrap()).collect();
        for tool in ["Glob", "Grep", "Read", "Write", "Edit", "mcp__memory__memory_recent"] {
            assert!(records.iter().any(|record| record["provider"] == "codex" && record["tool"] == tool
                && record["stdout_truncated"].as_str().is_some_and(|text| !text.is_empty())
                && record["stderr_truncated"].as_str().is_none_or(|text| text.is_empty())),
                "missing successful audited {tool}");
        }
    }

    #[tokio::test]
    #[ignore = "requires a logged-in Codex CLI; reads a synthetic image only"]
    async fn live_scoped_image_read_is_visible_to_codex() {
        let dir = tempfile::tempdir().unwrap();
        // Synthetic solid-color PNG; prompt and filename do not reveal color.
        let original: &[u8] = &[137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 8, 0, 0, 0, 8, 8, 2, 0, 0, 0, 75, 109, 41, 220, 0, 0, 0, 16, 73, 68, 65, 84, 120, 156, 99, 96, 96, 248, 143, 3, 13, 41, 9, 0, 169, 112, 63, 193, 20, 202, 234, 115, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130];
        let path = dir.path().join("fixture.bin");
        std::fs::write(&path, original).unwrap();
        let mut options = crate::reasoner::resume_opts(dir.path().into());
        options.allowed_tools = vec!["Read".into()];
        options.system_prompt = "Read the supplied synthetic fixture with the Jarvis Read tool and answer from the actual image.".into();
        let audit = dir.path().join("audit.jsonl");
        options.audit_logger = Some(std::sync::Arc::new(crate::tool_audit::AuditLogger::new(audit.clone())));
        let result = CodexCliReasoner::openai().call(&options,
            "Use Jarvis Read on fixture.bin. What single color fills the image? Return only COLOR=<color name>.").await.unwrap();
        assert_eq!(result.trim().to_ascii_lowercase(), "color=blue");
        assert_eq!(std::fs::read(&path).unwrap(), original);
        let records = std::fs::read_to_string(audit).unwrap();
        assert!(records.lines().filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .any(|r| r["tool"] == "Read" && r["provider"] == "codex"));
    }

    #[tokio::test]
    #[ignore = "requires a logged-in Codex CLI and Poppler; reads a synthetic PDF only"]
    async fn live_scoped_pdf_page_read_is_visible_to_codex() {
        let dir = tempfile::tempdir().unwrap();
        let original = include_bytes!("../../../scripts/tests/fixtures/scoped-document.pdf");
        let path = dir.path().join("document.pdf");
        std::fs::write(&path, original).unwrap();
        let mut options = crate::reasoner::resume_opts(dir.path().into());
        options.allowed_tools = vec!["Read".into()];
        options.system_prompt = "Read the requested PDF page with the Jarvis Read tool and answer from the actual rendered page.".into();
        let result = CodexCliReasoner::openai().call(&options,
            "Use Jarvis Read on document.pdf with pages=2. What single color fills page 2? Return only COLOR=<color name>.").await.unwrap();
        assert_eq!(result.trim().to_ascii_lowercase(), "color=blue");
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[tokio::test]
    #[ignore = "requires Codex login and a configured VM runtime; builds synthetic code only"]
    async fn live_codex_builds_and_tests_with_the_vm_bridge() {
        assert_eq!(crate::codex_tools::build_runner().label(), "vm");
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("Cargo.toml"),
            "[package]\nname=\"synthetic-live-vm\"\nversion=\"0.1.0\"\nedition=\"2021\"\n").unwrap();
        let source = "#[test] fn loopback_works() { let _listener = std::net::TcpListener::bind(\"127.0.0.1:0\").unwrap(); }\n";
        std::fs::write(dir.path().join("src/lib.rs"), source).unwrap();
        let mut options = crate::reasoner::resume_opts(dir.path().into());
        options.allowed_tools.push("Bash(cargo *)".into());
        options.system_prompt = "Run the requested synthetic project's tests using the Jarvis Bash tool and report the actual result. Preserve the supplied test source.".into();
        let log_dir = tempfile::tempdir().unwrap();
        let audit = log_dir.path().join("audit.jsonl");
        options.audit_logger = Some(std::sync::Arc::new(crate::tool_audit::AuditLogger::new(audit.clone())));
        CodexCliReasoner::openai().call(&options,
            "Run cargo test --offline with Jarvis Bash, then report whether the test passed.").await.unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("src/lib.rs")).unwrap(), source);
        assert!(!dir.path().join("target").exists(), "build outputs must stay out of the source worktree");
        let records = std::fs::read_to_string(audit).unwrap();
        assert!(records.lines().filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .any(|r| r["tool"] == "Bash" && r["provider"] == "codex" && r["exit_code"] == 0
                && r["stdout_truncated"].as_str().is_some_and(|s| s.contains("1 passed"))));
    }

    #[tokio::test]
    #[ignore = "requires Codex login and public web access; reads example.com only"]
    async fn live_native_web_call_reaches_common_audit_log() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("audit.jsonl");
        let mut options = crate::reasoner::resume_opts(dir.path().into());
        options.allowed_tools = vec!["WebSearch".into(), "WebFetch".into()];
        options.system_prompt = "Use the native web tool for the requested public page.".into();
        options.audit_logger = Some(std::sync::Arc::new(crate::tool_audit::AuditLogger::new(log.clone())));
        let response = CodexCliReasoner::openai().call(&options,
            "Open https://example.com with the web tool and report its heading.").await.unwrap();
        assert!(response.contains("Example Domain"), "{response}");
        let records: Vec<serde_json::Value> = std::fs::read_to_string(log).unwrap().lines()
            .map(|line| serde_json::from_str(line).unwrap()).collect();
        assert!(records.iter().any(|record| record["provider"] == "codex"
            && record["tool"] == "WebSearch"
            && record["args"].to_string().contains("example.com")));
    }

    #[tokio::test]
    async fn native_web_events_are_audited_without_inventing_response_content() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("audit.jsonl");
        let mut options = crate::reasoner::resume_opts(dir.path().into());
        options.audit_logger = Some(std::sync::Arc::new(crate::tool_audit::AuditLogger::new(log.clone())));
        // Observed CLI completion schema: native fetches also use web_search
        // with an opaque `other` action and no page response in the event.
        record_tool_item(&options, &serde_json::json!({
            "type": "web_search", "query": "https://example.com",
            "action": {"type": "other"}
        })).await;
        let record: serde_json::Value = serde_json::from_str(std::fs::read_to_string(log).unwrap().trim()).unwrap();
        assert_eq!(record["tool"], "WebSearch");
        assert_eq!(record["provider"], "codex");
        assert_eq!(record["args"]["query"], "https://example.com");
        assert_eq!(record["args"]["action"]["type"], "other");
        assert_eq!(record["stdout_truncated"], "");
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
        CodexCliReasoner { bin, gate: crate::cli_gate::CliGate::global(), usage_log: crate::token_usage::UsageLogger::global(), }
            .call(&options, "synthetic audit probe").await.unwrap();
        let row: serde_json::Value = serde_json::from_str(std::fs::read_to_string(log).unwrap().trim()).unwrap();
        assert_eq!(row["provider"], "codex");
        assert_eq!(row["tool"], "Write");
        assert_eq!(row["session_id"], "synthetic-session");
        assert_eq!(row["stdout_truncated"], "written");
    }

    /// #1047 — a Codex-served call on a preset with no audit logger (every
    /// preset but `ask_opts`) still writes a provider=codex row to the
    /// default log, as the Claude adapter has since #1004. Holds the audit
    /// env lock so the toggle test cannot switch auditing off mid-call.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn codex_tool_calls_reach_the_default_log_when_the_preset_passes_no_logger() {
        let _env = crate::reasoner::audit_env_guard();
        let prev = std::env::var("AUGMENTAGENT_TOOL_AUDIT").ok();
        std::env::remove_var("AUGMENTAGENT_TOOL_AUDIT");
        // The unit-test binary's state dir is a private scratch dir (or the
        // caller's XDG_STATE_HOME), never the owner's live log.
        let log = crate::tool_audit::default_audit_log_path();
        let real = crate::state_dir::resolve(None, std::env::var_os("HOME"));
        assert!(real.is_none_or(|real| !log.starts_with(real)), "{log:?}");

        let dir = tempfile::tempdir().unwrap();
        let session = format!("synthetic-default-log-{}", std::process::id());
        let bin = stub(&dir, "fake-codex-default-audit", r#"
cat >/dev/null
echo '{"type":"item.completed","item":{"type":"mcp_tool_call","server":"jarvis","tool":"Read","arguments":{"file_path":"note.md"},"result":{"content":[{"type":"text","text":"synthetic"}]},"status":"completed"}}'
echo '{"type":"item.completed","item":{"type":"agent_message","text":"ok"}}'
"#);
        let mut options = crate::reasoner::triage_opts(None);
        assert!(options.audit_logger.is_none(), "the scenario needs a preset without a logger");
        options.session_id = Some(session.clone());
        let result = CodexCliReasoner::with_bin(bin).call(&options, "synthetic request").await;
        match prev {
            Some(v) => std::env::set_var("AUGMENTAGENT_TOOL_AUDIT", v),
            None => std::env::remove_var("AUGMENTAGENT_TOOL_AUDIT"),
        }
        result.unwrap();
        let body = std::fs::read_to_string(&log).expect("default audit log written");
        let rows: Vec<serde_json::Value> = body.lines().filter_map(|l| serde_json::from_str(l).ok())
            .filter(|r: &serde_json::Value| r["session_id"] == session.as_str()).collect();
        assert_eq!(rows.len(), 1, "{body}");
        assert_eq!(rows[0]["provider"], "codex");
        assert_eq!(rows[0]["tool"], "Read");
    }

    #[tokio::test]
    async fn codex_build_audit_records_name_the_bridge_runner() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("audit.jsonl");
        let bin = stub(&dir, "fake-codex-build-audit", r#"
cat >/dev/null
echo '{"type":"item.completed","item":{"type":"mcp_tool_call","server":"jarvis","tool":"Bash","arguments":{"command":"\"cargo\" test"},"result":{"content":[{"type":"text","text":"{\"runner\": \"vm\", \"exit_code\": 0, \"stdout\": \"\", \"stderr\": \"\"}"}]},"status":"completed"}}'
echo '{"type":"item.completed","item":{"type":"mcp_tool_call","server":"jarvis","tool":"Bash","arguments":{"command":"npm test"},"result":{"isError":true,"content":[{"type":"text","text":"JARVIS_READINESS:build_vm_unavailable synthetic"}]},"status":"completed"}}'
echo '{"type":"item.completed","item":{"type":"mcp_tool_call","server":"jarvis","tool":"Bash","arguments":{"command":"git status"},"result":{"content":[{"type":"text","text":"{\"runner\": \"host\", \"exit_code\": 0, \"stdout\": \"\", \"stderr\": \"\"}"}]},"status":"completed"}}'
echo '{"type":"item.completed","item":{"type":"mcp_tool_call","server":"jarvis","tool":"Bash","arguments":{"command":"cargo test"},"result":{"isError":true,"content":[{"type":"text","text":"[runner=host] Operation denied or invalid for the configured profile."}]},"status":"completed"}}'
echo '{"type":"item.completed","item":{"type":"mcp_tool_call","server":"jarvis","tool":"Bash","arguments":{"command":"cargo test"},"result":{"content":[{"type":"text","text":"{\"runner\": \"vm\", \"exit_code\": 0, \"stdout\": \"synthetic output cut mid-str"}]},"status":"completed"}}'
echo '{"type":"item.completed","item":{"type":"mcp_tool_call","server":"jarvis","tool":"Bash","arguments":{"command":"cargo test"},"result":{"isError":true,"content":[{"type":"text","text":"Operation denied or invalid for the configured profile."}]},"status":"completed"}}'
echo '{"type":"item.completed","item":{"type":"agent_message","text":"ok"}}'
"#);
        let mut options = opts();
        options.audit_logger = Some(std::sync::Arc::new(crate::tool_audit::AuditLogger::new(log.clone())));
        CodexCliReasoner { bin, gate: crate::cli_gate::CliGate::global(), usage_log: crate::token_usage::UsageLogger::global() }
            .call(&options, "synthetic build audit probe").await.unwrap();
        let rows: Vec<serde_json::Value> = std::fs::read_to_string(log).unwrap().lines()
            .map(|line| serde_json::from_str(line).unwrap()).collect();
        assert_eq!(rows.len(), 6);
        // The runner comes from the bridge, never from re-parsing the command.
        assert_eq!(rows[0]["runner"], "vm", "quoted build command");
        assert_eq!(rows[0]["exit_code"], 0);
        assert_eq!(rows[1]["runner"], "none", "refused before any process started");
        assert_eq!(rows[2]["runner"], "host");
        assert_eq!(rows[3]["runner"], "host", "denied after the process started (timeout)");
        assert_eq!(rows[4]["runner"], "vm", "truncated result keeps its leading runner");
        assert_eq!(rows[5]["runner"], "none");
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
            gate: crate::cli_gate::CliGate::global(), usage_log: crate::token_usage::UsageLogger::global(),
        };
        let err = r.call(&opts(), "hi").await.unwrap_err();
        match ReasonerError::find_in(&err) {
            Some(ReasonerError::RateLimited { provider, .. }) => assert_eq!(provider, "codex"),
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    /// #1040 C1 — a finished turn with no final `agent_message` is
    /// content-level, like claude's `EmptyOutput`: untyped, so the chain
    /// neither latches codex nor re-runs the call elsewhere. A transient
    /// reconnect notice on a turn that then completed changes nothing.
    #[tokio::test]
    async fn empty_successful_output_is_content_level_not_a_provider_outage() {
        let dir = tempfile::tempdir().unwrap();
        let bin = stub(&dir, "fake-codex-empty", r#"
cat >/dev/null
echo '{"type":"thread.started","thread_id":"t1"}'
echo '{"type":"error","message":"Reconnecting... 1/5 (stream disconnected before completion)"}'
echo '{"type":"item.completed","item":{"type":"mcp_tool_call","server":"jarvis","tool":"Write","arguments":{"file_path":"synthetic.md"},"result":{"content":[{"type":"text","text":"written"}]},"status":"completed"}}'
echo '{"type":"turn.completed","usage":{"input_tokens":10}}'
"#);
        let reasoner = CodexCliReasoner { bin, gate: crate::cli_gate::CliGate::global(), usage_log: crate::token_usage::UsageLogger::global(), };
        for transcript in [false, true] {
            let err = if transcript {
                reasoner.call_transcript(&opts(), "synthetic request").await.unwrap_err()
            } else {
                reasoner.call(&opts(), "synthetic request").await.unwrap_err()
            };
            assert!(ReasonerError::find_in(&err).is_none(), "must stay untyped: {err:#}");
            assert_eq!(err.to_string(), "codex produced no assistant text");
            assert_eq!(err.downcast_ref::<TurnFailure>().map(|f| f.class), Some(FailureClass::Content));
        }
    }

    /// #1040 C2 — one `turn.failed` per failure class, end to end through a
    /// stub CLI on a text-only call, pins the adapter's wiring into
    /// `crate::turn_failure`. Each wording is pinned without a spawn in
    /// `turn_failure::tests`. `None` = untyped (no latch, no chain advance);
    /// `Some(true)` = RateLimited; `Some(false)` = Unavailable.
    #[tokio::test]
    async fn turn_failed_routing_is_pinned_per_failure_class() {
        let cases: &[(&str, Option<bool>)] = &[
            // Content-level: the provider is healthy, the turn is not.
            ("Codex ran out of room in the model's context window. Start a new thread or clear earlier history before retrying.", None),
            // Unrecognised on a text-only call: an outage (#1069 review H1).
            // The write-class fail-safe is pinned in
            // `unexplained_failures_after_the_turn_began_route_by_capability`.
            ("synthetic unrecognised failure 7F3A", Some(false)),
            // Quota walls (the pre-#1040 behaviour, kept), and the plan wall.
            ("exceeded retry limit, last status: 429 Too Many Requests", Some(true)),
            ("To use Codex with your ChatGPT plan, upgrade to Plus: https://chatgpt.com/explore/plus.", Some(true)),
            // Transport and auth: still provider outages.
            ("stream disconnected before completion: error sending request", Some(false)),
            ("unexpected status 520 <html>cloudflare</html>", Some(false)),
            ("unexpected status 401 Unauthorized: missing bearer", Some(false)),
        ];
        let dir = tempfile::tempdir().unwrap();
        for (index, (message, want)) in cases.iter().enumerate() {
            let events = dir.path().join(format!("events-{index}.jsonl"));
            let lines = [
                serde_json::json!({"type": "thread.started", "thread_id": "t1"}),
                serde_json::json!({"type": "turn.started"}),
                serde_json::json!({"type": "turn.failed", "error": {"message": message}}),
            ];
            std::fs::write(&events, lines.iter().map(|l| format!("{l}\n")).collect::<String>()).unwrap();
            let bin = stub(&dir, &format!("fake-codex-{index}"),
                &format!("cat >/dev/null\ncat '{}'\nexit 1\n", events.display()));
            let reasoner = CodexCliReasoner { bin, gate: crate::cli_gate::CliGate::global(), usage_log: crate::token_usage::UsageLogger::global(), };
            let err = reasoner.call(&opts(), "synthetic request").await.unwrap_err();
            match (want, ReasonerError::find_in(&err)) {
                (None, None) => {}
                (Some(true), Some(ReasonerError::RateLimited { provider, .. })) => assert_eq!(provider, "codex"),
                (Some(false), Some(ReasonerError::Unavailable { provider, .. })) => assert_eq!(provider, "codex"),
                (want, got) => panic!("{message:?}: wanted {want:?}, got {got:?} ({err:#})"),
            }
        }
    }

    /// A write-capable request for the adapter (ingest-shaped: WriteTools).
    /// The real ingest preset now ships the #1094 journal-guard hook, which
    /// classifies FullAgentic (hooks are honored only by the Claude CLI) —
    /// strip it here because this test exercises the WriteTools routing.
    fn write_opts(dir: &tempfile::TempDir) -> ReasonerOpts {
        let mut options = crate::reasoner::ingest_opts("Synthetic ingestion".into(), dir.path().into());
        options.settings_json = None;
        assert_eq!(crate::providers::classify(&options), crate::providers::CapabilityClass::WriteTools);
        options
    }

    /// #1040 — once the turn began, a failure nothing explains is not
    /// evidence of an outage: a message-less `turn.failed`, or a process
    /// that died mid-turn without a word (the supervisor reports a signal
    /// death as a plain exit 1, so it is indistinguishable from any other
    /// silent exit). For a write-capable call tools may already have run, so
    /// fail safe: untyped. #1069 review H1: a text-only or read-only call
    /// cannot repeat a write, and an untyped ending there would respawn codex
    /// on every call during an outage the table does not know, so it is an
    /// outage (latch and fail over).
    #[tokio::test]
    async fn unexplained_failures_after_the_turn_began_route_by_capability() {
        let dir = tempfile::tempdir().unwrap();
        let mut read_opts = opts();
        read_opts.allowed_tools = vec!["Read".into(), "Grep".into()];
        for (name, body) in [
            ("fake-codex-bare-failure", "cat >/dev/null\necho '{\"type\":\"turn.started\"}'\necho '{\"type\":\"turn.failed\",\"error\":{}}'\nexit 1\n"),
            ("fake-codex-killed", "cat >/dev/null\necho '{\"type\":\"turn.started\"}'\nkill -9 $$\n"),
            ("fake-codex-model", "cat >/dev/null\necho '{\"type\":\"turn.started\"}'\necho '{\"type\":\"turn.failed\",\"error\":{\"message\":\"unexpected status 404 Not Found: model_not_found\"}}'\nexit 1\n"),
        ] {
            let reasoner = CodexCliReasoner { bin: stub(&dir, name, body), gate: crate::cli_gate::CliGate::global(), usage_log: crate::token_usage::UsageLogger::global(), };
            let err = reasoner.call(&write_opts(&dir), "synthetic request").await.unwrap_err();
            assert!(ReasonerError::find_in(&err).is_none(), "{name} (write): {err:#}");
            assert_eq!(err.downcast_ref::<TurnFailure>().map(|f| f.class), Some(FailureClass::Unrecognised), "{name}");
            for (label, options) in [("text", opts()), ("read", read_opts.clone())] {
                let err = reasoner.call(&options, "synthetic request").await.unwrap_err();
                assert!(matches!(ReasonerError::find_in(&err), Some(ReasonerError::Unavailable { .. })),
                    "{name} ({label}): {err:#}");
            }
        }
    }

    /// #1069 review M2 — exit 0 with no turn event at all is not "the turn
    /// finished and said nothing": no turn ran, so it is the binary failing
    /// before the turn (fail over), whatever the capability class.
    #[tokio::test]
    async fn exit_zero_before_any_turn_event_is_a_binary_failure() {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in [
            ("fake-codex-silent", "cat >/dev/null\n"),
            ("fake-codex-thread-only", "cat >/dev/null\necho '{\"type\":\"thread.started\",\"thread_id\":\"t1\"}'\n"),
        ] {
            let reasoner = CodexCliReasoner { bin: stub(&dir, name, body), gate: crate::cli_gate::CliGate::global(), usage_log: crate::token_usage::UsageLogger::global(), };
            for options in [opts(), write_opts(&dir)] {
                let err = reasoner.call(&options, "synthetic request").await.unwrap_err();
                assert!(matches!(ReasonerError::find_in(&err), Some(ReasonerError::Unavailable { .. })),
                    "{name}: {err:#}");
            }
        }
    }

    /// #1040 — binary failures keep latching: a codex that exits non-zero
    /// before any turn event (nothing can have run), or one that panicked,
    /// is the provider's process failing, not the request's content.
    #[tokio::test]
    async fn binary_failures_still_map_to_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in [
            ("fake-codex-startup", "cat >/dev/null\necho 'Error: synthetic startup failure' >&2\nexit 1\n"),
            ("fake-codex-panic", "cat >/dev/null\necho '{\"type\":\"turn.started\"}'\necho \"thread 'main' panicked at core/src/synthetic.rs:1:1:\" >&2\nexit 101\n"),
        ] {
            let bin = stub(&dir, name, body);
            let err = CodexCliReasoner { bin, gate: crate::cli_gate::CliGate::global(), usage_log: crate::token_usage::UsageLogger::global(), }
                .call(&opts(), "synthetic request").await.unwrap_err();
            assert!(matches!(ReasonerError::find_in(&err), Some(ReasonerError::Unavailable { .. })),
                "{name}: {err:#}");
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
            gate: crate::cli_gate::CliGate::global(), usage_log: crate::token_usage::UsageLogger::global(),
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
            gate: crate::cli_gate::CliGate::global(), usage_log: crate::token_usage::UsageLogger::global(),
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
            gate: crate::cli_gate::CliGate::global(), usage_log: crate::token_usage::UsageLogger::global(),
        };
        let err = r.call(&opts(), "hi").await.unwrap_err();
        assert!(matches!(
            ReasonerError::find_in(&err),
            Some(ReasonerError::Local { .. })
        ));
    }

    /// Stands in for Codex: starts the packaged bridge exactly as the
    /// `mcp_servers.jarvis` override says, sends one Grep and reports how long
    /// the reply took. It gives up after 30 s so a stalled bridge fails the test
    /// instead of hanging it.
    const FAKE_CODEX_PATHOLOGICAL_GREP: &str = r##"
cat >/dev/null
exec python3 -I - "$@" <<'PY'
import json, os, select, subprocess, sys, time
spec = next(arg for arg in sys.argv[1:] if arg.startswith('mcp_servers.jarvis='))
args = json.loads('[' + spec.split('args=[', 1)[1].split(']', 1)[0] + ']')
bridge = subprocess.Popen(['python3', *args], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
pending = b''
def call(identifier, method, params, seconds):
    global pending
    bridge.stdin.write((json.dumps({'jsonrpc': '2.0', 'id': identifier, 'method': method, 'params': params}) + '\n').encode())
    bridge.stdin.flush()
    deadline = time.monotonic() + seconds
    while b'\n' not in pending:
        remaining = deadline - time.monotonic()
        if remaining <= 0 or not select.select([bridge.stdout], [], [], remaining)[0]:
            return None
        chunk = os.read(bridge.stdout.fileno(), 65536)
        if not chunk:
            return None
        pending += chunk
    line, pending = pending.split(b'\n', 1)
    return json.loads(line)
call(1, 'initialize', {}, 30)
started = time.monotonic()
reply = call(2, 'tools/call', {'name': 'Grep', 'arguments': {'pattern': '(a+)+$'}}, 30)
elapsed = time.monotonic() - started
if reply is None:
    bridge.kill()
bridge.stdin.close()
bridge.wait()
report = json.dumps({'grep_seconds': elapsed, 'reply': reply})
print(json.dumps({'type': 'item.completed', 'item': {'type': 'agent_message', 'text': report}}), flush=True)
PY
"##;

    /// #1038 C3: a pathological Grep through the real packaged bridge is
    /// answered within its bound. The Codex turn then ends normally and its
    /// CLI-gate slot is released, instead of being held until the watchdog.
    #[tokio::test]
    async fn pathological_bridge_grep_releases_the_cli_gate_slot_within_its_bound() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        // Forty characters of backtracking bait for Python's re engine.
        std::fs::write(workspace.join("bait.txt"), format!("{}b\n", "a".repeat(40))).unwrap();
        let bin = stub(&dir, "fake-codex-grep", FAKE_CODEX_PATHOLOGICAL_GREP);
        let gate = std::sync::Arc::new(crate::cli_gate::CliGate::new(1));
        let reasoner = CodexCliReasoner { bin, gate: gate.clone(), usage_log: crate::token_usage::UsageLogger::global() };
        let mut options = opts();
        options.allowed_tools = vec!["Grep".into()];
        options.cwd = Some(workspace);
        let started = std::time::Instant::now();
        let answer = reasoner.call(&options, "Synthetic pathological search").await.unwrap();
        let held = started.elapsed();
        let report: serde_json::Value = serde_json::from_str(answer.trim()).unwrap();
        assert_eq!(report["reply"]["result"]["isError"], true, "{report}");
        assert!(report["reply"]["result"]["content"][0]["text"].as_str().unwrap_or_default()
            .contains("time limit"), "{report}");
        assert!(report["grep_seconds"].as_f64().unwrap() < 2.0, "{report}");
        assert!(held < std::time::Duration::from_secs(20), "gate slot held for {held:?}");
        assert_eq!(gate.in_flight(), 0);
        let next = gate.acquire_timed("codex", "synthetic-next-call", std::time::Duration::from_millis(500)).await;
        assert!(next.is_ok(), "the only gate slot was not released");
    }

}
