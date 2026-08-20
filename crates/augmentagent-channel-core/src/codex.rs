//! Codex CLI reasoner adapter (#655/#661).
//!
//! Translates the same [`ReasonerOpts`] every preset already builds into a
//! `codex exec` spawn:
//!
//! ```text
//! CODEX_HOME=<home> codex exec --json --skip-git-repo-check \
//!     --ignore-user-config -C <cwd> -s read-only \
//!     -c approval_policy=never -c model_instructions_file=<tmp>/instructions.md \
//!     -m <model> -
//! ```
//!
//! Design notes (researched 2026-08-19, developers.openai.com/codex):
//!
//! - **No `--system-prompt` flag**: the preset's system prompt is written to
//!   a temp file and wired via `-c model_instructions_file=…`, which fully
//!   replaces codex's base instructions. The spawn `cwd` is pinned, and we
//!   pass `--ignore-user-config` so neither the owner's interactive
//!   `~/.codex/config.toml` nor an AGENTS.md in the daemon repo can leak
//!   into background calls (the #448 leak class, codex edition).
//! - **Sandbox `read-only` always**: the eligibility policy only routes
//!   text-only and read-tools presets here (#658), and codex's kernel
//!   sandbox (Landlock) enforcing "no writes, no network for commands" is
//!   strictly stronger than what those presets grant Claude.
//! - **Auth**: `CODEX_API_KEY` (JIT keyring load, honored only by
//!   `codex exec`) when present, else the `auth.json` under the resolved
//!   CODEX_HOME (ChatGPT-plan login; token refresh writes back because the
//!   home is persistent, not a per-spawn tempdir).
//!   (NB: the Cerebras tier was planned to ride this adapter via a custom
//!   `model_provider`, but codex ≥0.148 removed `wire_api = "chat"` and
//!   Cerebras has no Responses API — verified live 2026-08-19. Cerebras is a
//!   thin chat-completions client instead: see `crate::cerebras`, #663.)
//! - **Failure mapping**: quota exhaustion surfaces as `turn.failed` (+
//!   non-zero exit) with a usage-limit message — mapped to
//!   [`ReasonerError::RateLimited`]. Connection/5xx → `Unavailable`.
//!   Missing binary → `Local`. All under the #656 watchdog with
//!   `kill_on_drop`.

use std::process::Stdio;

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tracing::{debug, warn};

use crate::providers::{model_for, tier_of, ProviderKind};
use crate::reasoner::{
    parse_reset_hint, reasoner_timeout, Reasoner, ReasonerError, ReasonerOpts,
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
}

impl CodexCliReasoner {
    pub fn openai() -> Self {
        Self { bin: codex_bin() }
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
        match tokio::time::timeout(dur, self.call_once(opts, user_message, all_blocks)).await {
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
            "-s".into(),
            "read-only".into(),
            "-c".into(),
            "approval_policy=never".into(),
            "-c".into(),
            format!("model_instructions_file={}", instructions.display()),
            "-m".into(),
            model.clone(),
        ];
        // cwd: preset pin wins; else the first --add-dir (wiki root for the
        // read-tools presets); else the daemon cwd.
        let cwd = opts
            .cwd
            .clone()
            .or_else(|| opts.add_dirs.first().cloned())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into()));
        args.push("-C".into());
        args.push(cwd.to_string_lossy().into_owned());
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
        if !opts.env.is_empty() {
            cmd.envs(opts.env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        }

        let mut child = cmd.spawn().map_err(|e| {
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
        let mut stderr_buf = String::new();
        if let Some(mut err) = child.stderr.take() {
            use tokio::io::AsyncReadExt;
            let _ = err.read_to_string(&mut stderr_buf).await;
        }

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
    d.contains("usage limit")
        || d.contains("rate limit")
        || d.contains("rate_limit")
        || d.contains("insufficient_quota")
        || d.contains("resource_exhausted")
        || d.contains("429")
        || d.contains("402")
        || d.contains("payment required")
        || d.contains("quota")
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
        let r = CodexCliReasoner { bin };
        let got = r.call(&opts(), "classify this").await.unwrap();
        assert_eq!(got, "{\"decision\":\"reply\"}", "LastBlock keeps the final message");
        let all = r.call_transcript(&opts(), "classify this").await.unwrap();
        assert!(all.contains("scratch note") && all.contains("decision"));
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
        let r = CodexCliReasoner { bin };
        let err = r.call(&opts(), "hi").await.unwrap_err();
        match ReasonerError::find_in(&err) {
            Some(ReasonerError::RateLimited { provider, .. }) => assert_eq!(provider, "codex"),
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_binary_is_local_not_failoverable_noise() {
        let r = CodexCliReasoner {
            bin: "/nonexistent/codex-bin".into(),
        };
        let err = r.call(&opts(), "hi").await.unwrap_err();
        assert!(matches!(
            ReasonerError::find_in(&err),
            Some(ReasonerError::Local { .. })
        ));
    }

}
