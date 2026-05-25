//! Direct `claude` CLI reasoner.
//!
//! Spawns the `claude` binary with `-p --output-format stream-json` and reads
//! the final assistant text. Authenticated via the user's Claude Max
//! subscription session (`claude login`); no API key.

use std::path::PathBuf;
use std::process::Stdio;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tracing::{debug, warn};

/// Per-call options for a `Reasoner`. Each call type (triage, draft, ingest)
/// gets a different preset — see `triage_opts`, `draft_opts`, `ingest_opts`.
#[derive(Debug, Clone)]
pub struct ReasonerOpts {
    pub system_prompt: String,
    pub model: Option<String>,
    pub allowed_tools: Vec<String>,
    pub add_dirs: Vec<PathBuf>,
    pub permission_mode: String,
    /// Override the spawned Claude CLI's working directory. Useful to scope
    /// Write/Edit to a specific subtree (e.g. wiki root) so accidental writes
    /// can't escape into the source tree.
    pub cwd: Option<PathBuf>,
    /// Extra env vars to set on the spawned Claude CLI process. Inherited by
    /// any sub-processes Claude itself spawns (e.g. `augmentagent gmail
    /// search`). Used to pass `AUGMENTAGENT_DB` so sub-CLIs find the db even
    /// when `cwd` is pinned to a sibling directory like the wiki root.
    pub env: Vec<(String, String)>,
}

/// Trait the channel uses to reach Claude. Test doubles stub this.
///
/// `call_code_mode` and `call_code_mode_with_repair` are sibling entrypoints
/// that reuse the same `claude` CLI machinery as `call`. They are provided
/// as default trait methods so test doubles only need to stub `call` — the
/// fenced-block extraction layer lives here, once.
#[async_trait]
pub trait Reasoner: Send + Sync {
    async fn call(&self, opts: &ReasonerOpts, user_message: &str) -> anyhow::Result<String>;

    /// Code-Mode entrypoint. Spawns `claude` exactly like [`Reasoner::call`]
    /// using the caller-provided system prompt (which should be built via
    /// [`crate::prompt::code_mode_system`] from a manifest), parses the
    /// stream-json output identically, then extracts the first fenced
    /// ```ts``` or ```typescript``` block from the assistant text.
    ///
    /// Returns the source string (trimmed) on success. Returns
    /// [`crate::code_mode::CodeModeError::NoCodeBlock`] (wrapped in
    /// `anyhow::Error`) if the response has no fenced ts block.
    async fn call_code_mode(
        &self,
        opts: &ReasonerOpts,
        user_message: &str,
    ) -> anyhow::Result<String> {
        // Wrap the inner `call` error in `CodeModeError::ReasonerFailed` so
        // callers can distinguish "claude itself failed" from "claude returned
        // text but it had no fenced block".
        let raw = match self.call(opts, user_message).await {
            Ok(t) => t,
            Err(e) => return Err(crate::code_mode::CodeModeError::ReasonerFailed(e).into()),
        };
        let source = crate::code_mode::extract_ts_block(&raw)?;
        Ok(source)
    }

    /// Self-repair retry. Same wiring as [`Reasoner::call_code_mode`] but
    /// the caller supplies the prior failed source and the error message,
    /// which are appended to `user_message` before sending. The base
    /// `user_message` should be the same string passed to the first
    /// `call_code_mode` attempt so the cache prefix is preserved.
    async fn call_code_mode_with_repair(
        &self,
        opts: &ReasonerOpts,
        user_message: &str,
        prior_source: &str,
        prior_error: &str,
    ) -> anyhow::Result<String> {
        // Repair tail is byte-identical to the one in
        // `crate::prompt::code_mode_repair_user_message` so the two ways of
        // assembling a repair message (here in the reasoner, or pre-built
        // by the caller via `code_mode_repair_user_message`) produce the
        // same wire bytes given the same base message.
        let repair_msg = format!(
            "{user_message}\nThe previous attempt failed. Read the program and the error, then output a corrected program. Same hard rules apply.\n\n<prior_program>\n{prior_source}\n</prior_program>\n\n<prior_error>\n{prior_error}\n</prior_error>\n\nReturn ONLY a single fenced ```ts code block containing the corrected program.\n"
        );
        self.call_code_mode(opts, &repair_msg).await
    }
}

pub struct ClaudeCliReasoner {
    bin: String,
}

impl Default for ClaudeCliReasoner {
    fn default() -> Self {
        Self {
            bin: std::env::var("CLAUDE_CLI").unwrap_or_else(|_| "claude".into()),
        }
    }
}

impl ClaudeCliReasoner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Legacy convenience: pin a model for callers that don't build a full
    /// `ReasonerOpts` themselves. Kept only as a construction helper; model
    /// is applied via `ReasonerOpts::model` now.
    pub fn with_model(self, _model: impl Into<String>) -> Self {
        self
    }
}

#[async_trait]
impl Reasoner for ClaudeCliReasoner {
    async fn call(&self, opts: &ReasonerOpts, user_message: &str) -> anyhow::Result<String> {
        let mut args: Vec<String> = vec![
            "-p".into(),
            "--output-format".into(),
            "stream-json".into(),
            "--verbose".into(),
            "--permission-mode".into(),
            opts.permission_mode.clone(),
        ];

        // `--allowedTools`: empty list = no tools. Space-separated names per claude CLI docs.
        args.push("--allowedTools".into());
        args.push(opts.allowed_tools.join(" "));

        // `--system-prompt` REPLACES Claude Code's default system (tools list,
        // MCP metadata, agent catalog — ~25k tokens). Empty → minimal stub.
        let effective_system = if opts.system_prompt.trim().is_empty() {
            "You are a concise assistant."
        } else {
            &opts.system_prompt
        };
        args.push("--system-prompt".into());
        args.push(effective_system.to_string());

        if let Some(m) = &opts.model {
            args.push("--model".into());
            args.push(m.clone());
        }

        for dir in &opts.add_dirs {
            args.push("--add-dir".into());
            args.push(dir.to_string_lossy().into_owned());
        }

        let mut cmd = Command::new(&self.bin);
        cmd.args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Scope Write/Edit by setting the spawned CLI's cwd when requested.
        if let Some(cwd) = &opts.cwd {
            cmd.current_dir(cwd);
        }
        // Forward any extra env vars. These are inherited by any subprocesses
        // Claude spawns (notably `augmentagent gmail search` via the scoped
        // Bash allowlist in `ask_opts`), which is how we carry `AUGMENTAGENT_DB`
        // through to a sub-CLI whose cwd is unrelated to the repo root.
        if !opts.env.is_empty() {
            cmd.envs(opts.env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        }
        let mut child = cmd.spawn()?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(user_message.as_bytes()).await?;
            stdin.shutdown().await?;
        }

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("claude stdout missing"))?;
        let mut lines = BufReader::new(stdout).lines();

        let mut final_text = String::new();
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<StreamEvent>(&line) {
                Ok(StreamEvent::Assistant { message }) => {
                    for block in message.content {
                        if let ContentBlock::Text { text } = block {
                            final_text = text;
                        }
                    }
                }
                Ok(StreamEvent::Result { result }) => {
                    if let Some(r) = result {
                        if !r.trim().is_empty() {
                            final_text = r;
                        }
                    }
                }
                Ok(StreamEvent::Other) => {}
                Err(e) => debug!("stream-json parse skip: {e} line={line}"),
            }
        }

        let status = child.wait().await?;
        if !status.success() {
            let mut stderr_buf = String::new();
            if let Some(mut err) = child.stderr.take() {
                use tokio::io::AsyncReadExt;
                let _ = err.read_to_string(&mut stderr_buf).await;
            }
            warn!("claude exited {status:?}: {stderr_buf}");
            if final_text.is_empty() {
                return Err(anyhow::anyhow!(
                    "claude exited non-zero: {status:?}: {stderr_buf}"
                ));
            }
        }
        if final_text.is_empty() {
            return Err(anyhow::anyhow!("claude produced no assistant text"));
        }
        Ok(final_text)
    }
}

/// Resolve the absolute on-disk path of the app database so it can be passed
/// as `AUGMENTAGENT_DB` to sub-CLIs whose cwd may differ from the repo root.
///
/// Mirrors the resolution order in `main.rs`: explicit `AUGMENTAGENT_DB` env
/// wins, else `<repo_root>/data.db`. We always return an absolute path —
/// `canonicalize` when the file exists, otherwise fall back to a manual
/// absolute join so callers never inherit a relative path.
fn resolve_db_path(repo_root: &std::path::Path) -> PathBuf {
    let raw = std::env::var("AUGMENTAGENT_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|_| repo_root.join("data.db"));
    if let Ok(abs) = raw.canonicalize() {
        return abs;
    }
    if raw.is_absolute() {
        raw
    } else {
        repo_root.join(&raw)
    }
}

/// Preset builders for the three call types.
pub fn triage_opts(wiki_root: Option<PathBuf>) -> ReasonerOpts {
    // Opus for triage. Haiku was too narrow on "flag" — missed personal
    // messages from known contacts asking for engagement. Volume is ~70
    // emails/day so the cost bump is rounding error; this is the most
    // quality-critical step in the pipeline.
    let mut add_dirs = Vec::new();
    let mut allowed_tools = Vec::new();
    if let Some(root) = wiki_root {
        add_dirs.push(root);
        // Read-only access lets triage open a sender's people page and weight
        // importance by prior context without risking wiki mutation.
        allowed_tools = vec!["Read".into(), "Grep".into(), "Glob".into()];
    }
    ReasonerOpts {
        system_prompt: crate::prompt::TRIAGE_SYSTEM.to_string(),
        model: None,
        allowed_tools,
        add_dirs,
        permission_mode: "default".into(),
        cwd: None,
        env: Vec::new(),
    }
}

pub fn draft_opts(system_prompt: String, wiki_root: Option<PathBuf>) -> ReasonerOpts {
    let mut add_dirs = Vec::new();
    let mut allowed_tools = Vec::new();
    if let Some(root) = wiki_root {
        add_dirs.push(root);
        allowed_tools = vec!["Read".into(), "Grep".into(), "Glob".into()];
    }
    ReasonerOpts {
        system_prompt,
        model: None,
        allowed_tools,
        add_dirs,
        permission_mode: "default".into(),
        cwd: None,
        env: Vec::new(),
    }
}

pub fn lint_opts(system_prompt: String, wiki_root: PathBuf) -> ReasonerOpts {
    ReasonerOpts {
        system_prompt,
        model: None, // Opus — lint is reasoning-heavy, low volume
        allowed_tools: vec!["Read".into(), "Grep".into(), "Glob".into()],
        add_dirs: vec![wiki_root],
        permission_mode: "default".into(),
        cwd: None,
        env: Vec::new(),
    }
}

/// Preset for ad-hoc wiki queries (CLI `wiki ask` + Discord DMs).
///
/// Claude gets a broad toolbelt for this one — if the wiki doesn't answer, it
/// can search the inbox via the `augmentagent gmail search` subcommand (scoped
/// Bash allowlist), reach the web, and persist durable new facts back to the
/// wiki via Write/Edit. The spawned CLI's cwd is pinned to `wiki_root` so
/// Write/Edit cannot escape into the source tree.
pub fn ask_opts(wiki_root: PathBuf, repo_root: PathBuf) -> ReasonerOpts {
    let bin = repo_root.join("target/release/augmentagent");
    // Scoped Bash patterns: Claude can invoke our gmail subcommand via the
    // release binary's absolute path, plus `gh issue {create,list,view,comment}`
    // for filing AugmentAgent self-feedback issues. `/snap/bin/gh` is an
    // absolute path because the systemd unit's PATH does not include /snap/bin.
    // Anything else is denied by claude CLI.
    let bash_gmail = format!("Bash({} gmail *)", bin.display());
    // Invoice subcommands the LLM can autonomously invoke. `invoice run` is
    // intentionally absent — the only real-send path is the user clicking
    // Approve on a draft card. `status` and `list-accounts` omit the trailing
    // `*` because they take no args (clap would reject extras anyway).
    let bash_invoice_status = format!("Bash({} invoice status)", bin.display());
    let bash_invoice_draft = format!("Bash({} invoice draft *)", bin.display());
    let bash_invoice_list_accounts = format!("Bash({} invoice list-accounts)", bin.display());
    let bash_invoice_set_recipient = format!("Bash({} invoice set-recipient *)", bin.display());
    let bash_invoice_set_entity = format!("Bash({} invoice set-entity *)", bin.display());
    let bash_invoice_set_auto_draft = format!("Bash({} invoice set-auto-draft *)", bin.display());
    // The sub-CLI inherits our cwd = wiki_root, so its default `data.db`
    // lookup would fail. Ship an absolute `AUGMENTAGENT_DB` so `main.rs`
    // resolves the db regardless of cwd.
    let db_path = resolve_db_path(&repo_root);
    ReasonerOpts {
        system_prompt: include_str!("../../../schema/wiki-ask.md").to_string(),
        model: None, // Opus — quality matters for answer coherence
        allowed_tools: vec![
            "Read".into(),
            "Grep".into(),
            "Glob".into(),
            "Write".into(),
            "Edit".into(),
            "WebSearch".into(),
            "WebFetch".into(),
            bash_gmail,
            bash_invoice_status,
            bash_invoice_draft,
            bash_invoice_list_accounts,
            bash_invoice_set_recipient,
            bash_invoice_set_entity,
            bash_invoice_set_auto_draft,
            "Bash(/snap/bin/gh issue create *)".into(),
            "Bash(/snap/bin/gh issue list *)".into(),
            "Bash(/snap/bin/gh issue view *)".into(),
            "Bash(/snap/bin/gh issue comment *)".into(),
        ],
        add_dirs: vec![wiki_root.clone()],
        permission_mode: "acceptEdits".into(),
        // Pin cwd to the wiki so Write/Edit cannot touch the source tree.
        cwd: Some(wiki_root),
        env: vec![(
            "AUGMENTAGENT_DB".into(),
            db_path.to_string_lossy().into_owned(),
        )],
    }
}

/// Preset for the morning digest synthesis call.
/// System prompt embedded from `schema/digest-prompt.md`. Opus quality, wiki
/// read-only access so Claude can enrich bare stats with context from
/// people/project pages when the signal warrants it.
pub fn digest_opts(wiki_root: Option<PathBuf>) -> ReasonerOpts {
    let mut add_dirs = Vec::new();
    let mut allowed_tools = Vec::new();
    if let Some(root) = wiki_root {
        add_dirs.push(root);
        allowed_tools = vec!["Read".into(), "Grep".into(), "Glob".into()];
    }
    ReasonerOpts {
        system_prompt: include_str!("../../../schema/digest-prompt.md").to_string(),
        model: None, // Opus — digest tone + coverage benefit from quality
        allowed_tools,
        add_dirs,
        permission_mode: "default".into(),
        cwd: None,
        env: Vec::new(),
    }
}

/// Preset for the tone-mirroring summarizer (#73). Pure transform from a
/// corpus of sent-mail bodies to a ~120-token JSON voice descriptor — no
/// tools, no wiki access, Haiku for cost (per-recipient refresh is ~$0.001).
pub fn tone_summarize_opts() -> ReasonerOpts {
    ReasonerOpts {
        system_prompt: include_str!("../../../schema/tone-summarize.md").to_string(),
        model: Some("claude-haiku-4-5-20251001".into()),
        allowed_tools: vec![],
        add_dirs: vec![],
        permission_mode: "default".into(),
        cwd: None,
        env: vec![],
    }
}

/// Preset for the cross-platform content adapter (#53). One call per target
/// platform, fanned out in parallel. Pure text transform: no tools, no wiki
/// access, no Bash. The full system prompt (shared rules + the one platform's
/// section + optional voice sample) is assembled by the content-adapter crate
/// and passed in; we just pin the model + lock the toolbelt shut.
///
/// Opus, not Haiku: voice-matching + format nuance across platforms is
/// quality-sensitive and volume is tiny (a handful of variants per compose).
pub fn social_adapter_opts(system_prompt: String) -> ReasonerOpts {
    ReasonerOpts {
        system_prompt,
        model: None, // Opus — voice + format fidelity matter, volume is low
        allowed_tools: vec![],
        add_dirs: vec![],
        permission_mode: "default".into(),
        cwd: None,
        env: vec![],
    }
}

/// Preset for parsing Discord `/loop` create-text into a structured spec.
/// One JSON object out, no tools, Haiku for snappy command-feedback latency
/// (loop creates are interactive — sub-second matters). The system prompt
/// is inlined here since it's tiny and only used at one site.
pub fn loop_parse_opts() -> ReasonerOpts {
    ReasonerOpts {
        system_prompt: r#"You parse a single user request to create a recurring task ("loop").

The input describes:
- An interval (how often the loop fires) — REQUIRED
- A prompt (what the loop should do each tick) — REQUIRED
- Optionally, a total duration (auto-stop time)

Clauses may appear in any order. Recognised units: s/sec/seconds, m/min/minutes, h/hr/hours, d/day/days.

Output a SINGLE JSON object on one line, no prose, no code fences:
  {"interval_secs": <int>, "prompt": <string>, "duration_secs": <int or null>}

On failure (no parseable interval, ambiguous, or empty prompt), output:
  {"error": "<short user-facing message>"}

Examples:
  "5m do the digest" → {"interval_secs": 300, "prompt": "do the digest", "duration_secs": null}
  "say hi every 5 mins" → {"interval_secs": 300, "prompt": "say hi", "duration_secs": null}
  "every 5mins for 20 mins and say hello world 🙂" → {"interval_secs": 300, "prompt": "say hello world 🙂", "duration_secs": 1200}
  "ping me every 10 minutes for the next 2 hours" → {"interval_secs": 600, "prompt": "ping me", "duration_secs": 7200}
  "and say hello world every 5 mins for the next 15 mins" → {"interval_secs": 300, "prompt": "and say hello world", "duration_secs": 900}
  "triage every email every 1h" → {"interval_secs": 3600, "prompt": "triage every email", "duration_secs": null}
  "thirty seconds /digest" → {"interval_secs": 30, "prompt": "/digest", "duration_secs": null}
  "asdf" → {"error": "couldn't find an interval — try `loop 5m do thing` or `loop do thing every 5m`"}
"#.to_string(),
        model: Some("claude-haiku-4-5-20251001".into()),
        allowed_tools: vec![],
        add_dirs: vec![],
        permission_mode: "default".into(),
        cwd: None,
        env: vec![],
    }
}

/// Preset for the archetype picker (#36). A single fast structured-output
/// classification: email + triage label in, one archetype id (or `none`) +
/// confidence out. Haiku for cost/latency — the issue specifies a fast,
/// single-call classifier; no tools, no wiki.
pub fn archetype_pick_opts() -> ReasonerOpts {
    ReasonerOpts {
        system_prompt: crate::archetype::ARCHETYPE_PICKER_SYSTEM.to_string(),
        model: Some("claude-haiku-4-5-20251001".into()),
        allowed_tools: vec![],
        add_dirs: vec![],
        permission_mode: "default".into(),
        cwd: None,
        env: vec![],
    }
}

pub fn ingest_opts(system_prompt: String, wiki_root: PathBuf) -> ReasonerOpts {
    ReasonerOpts {
        system_prompt,
        model: Some("claude-haiku-4-5-20251001".into()),
        allowed_tools: vec![
            "Read".into(),
            "Grep".into(),
            "Glob".into(),
            "Write".into(),
            "Edit".into(),
        ],
        add_dirs: vec![wiki_root],
        permission_mode: "acceptEdits".into(),
        cwd: None,
        env: Vec::new(),
    }
}

/// Preset for the one-shot v2 wiki migration tool (`wiki migrate --to v2`).
///
/// Haiku for cost — full 562-page corpus runs ~$5. Read-only against the
/// wiki: the migration tool parses the model's text response and applies
/// the patch via Rust IO so v1 keys are preserved byte-for-byte (#78 §2
/// step 6 + §8 risk register forbid round-tripping the whole frontmatter).
/// cwd pinned so any accidental Bash escapes are scoped to the wiki tree.
pub fn wiki_migrate_opts(system_prompt: String, wiki_root: PathBuf) -> ReasonerOpts {
    ReasonerOpts {
        system_prompt,
        model: Some("claude-haiku-4-5-20251001".into()),
        allowed_tools: vec!["Read".into(), "Grep".into(), "Glob".into()],
        add_dirs: vec![wiki_root.clone()],
        permission_mode: "acceptEdits".into(),
        cwd: Some(wiki_root),
        env: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Test double that records the args of the last `call` and returns a
    /// canned string. Used to verify `call_code_mode` extracts the fenced
    /// block from whatever the underlying `call` returns, without spawning
    /// the real `claude` CLI.
    struct CannedReasoner {
        canned: String,
        last_user: Mutex<Option<String>>,
        last_system: Mutex<Option<String>>,
        fail: bool,
    }

    impl CannedReasoner {
        fn ok(canned: impl Into<String>) -> Self {
            Self {
                canned: canned.into(),
                last_user: Mutex::new(None),
                last_system: Mutex::new(None),
                fail: false,
            }
        }
        fn err() -> Self {
            Self {
                canned: String::new(),
                last_user: Mutex::new(None),
                last_system: Mutex::new(None),
                fail: true,
            }
        }
    }

    #[async_trait]
    impl Reasoner for CannedReasoner {
        async fn call(&self, opts: &ReasonerOpts, user_message: &str) -> anyhow::Result<String> {
            *self.last_user.lock().unwrap() = Some(user_message.to_string());
            *self.last_system.lock().unwrap() = Some(opts.system_prompt.clone());
            if self.fail {
                Err(anyhow::anyhow!("simulated claude failure"))
            } else {
                Ok(self.canned.clone())
            }
        }
    }

    fn dummy_opts() -> ReasonerOpts {
        ReasonerOpts {
            system_prompt: "stub".into(),
            model: None,
            allowed_tools: vec![],
            add_dirs: vec![],
            permission_mode: "default".into(),
            cwd: None,
            env: vec![],
        }
    }

    #[tokio::test]
    async fn call_code_mode_returns_fenced_ts_body_verbatim() {
        let canned = "Sure, here's the program:\n\n```ts\nasync function main(): Promise<void> {\n  await tools.draft(\"gmail\", \"hello\", \"reason\");\n}\nawait main();\n```\n";
        let reasoner = CannedReasoner::ok(canned);
        let got = reasoner
            .call_code_mode(&dummy_opts(), "user")
            .await
            .expect("must extract ts block");
        assert_eq!(
            got,
            "async function main(): Promise<void> {\n  await tools.draft(\"gmail\", \"hello\", \"reason\");\n}\nawait main();"
        );
    }

    #[tokio::test]
    async fn call_code_mode_errors_when_no_fenced_block() {
        let reasoner = CannedReasoner::ok("Sorry, I can't help with that.");
        let err = reasoner
            .call_code_mode(&dummy_opts(), "user")
            .await
            .expect_err("no code block must error");
        // Downcast through the anyhow chain to verify the typed error.
        let typed = err
            .downcast_ref::<crate::code_mode::CodeModeError>()
            .expect("error must be a CodeModeError");
        assert!(matches!(
            typed,
            crate::code_mode::CodeModeError::NoCodeBlock
        ));
    }

    #[tokio::test]
    async fn call_code_mode_wraps_underlying_call_failure() {
        let reasoner = CannedReasoner::err();
        let err = reasoner
            .call_code_mode(&dummy_opts(), "user")
            .await
            .expect_err("must surface call failure");
        let typed = err
            .downcast_ref::<crate::code_mode::CodeModeError>()
            .expect("error must be a CodeModeError");
        assert!(matches!(
            typed,
            crate::code_mode::CodeModeError::ReasonerFailed(_)
        ));
    }

    #[tokio::test]
    async fn call_code_mode_with_repair_appends_prior_program_and_error() {
        let canned = "```ts\nawait tools.draft(\"gmail\", \"fixed\", \"r\");\n```";
        let reasoner = CannedReasoner::ok(canned);
        let base_user = "Write a TypeScript program that drafts a reply.";
        let got = reasoner
            .call_code_mode_with_repair(
                &dummy_opts(),
                base_user,
                "async function main(){ throw new Error('boom') } main();",
                "Error: boom",
            )
            .await
            .expect("repair succeeds");
        assert_eq!(got, "await tools.draft(\"gmail\", \"fixed\", \"r\");");

        // The user message sent to `call` must include both the base text
        // and the failed program + error blocks.
        let sent = reasoner
            .last_user
            .lock()
            .unwrap()
            .clone()
            .expect("call must have been invoked");
        assert!(sent.contains(base_user), "missing base user msg: {sent}");
        assert!(sent.contains("<prior_program>"));
        assert!(sent.contains("throw new Error('boom')"));
        assert!(sent.contains("<prior_error>"));
        assert!(sent.contains("Error: boom"));
    }

    #[test]
    fn ask_opts_ships_absolute_db_env() {
        let repo = tempfile::tempdir().expect("repo tmpdir");
        let wiki = tempfile::tempdir().expect("wiki tmpdir");
        // Touch the db file so canonicalize succeeds, mirroring a real repo.
        std::fs::write(repo.path().join("data.db"), b"").unwrap();

        // Clear a potentially-inherited env that would override our resolution.
        let _guard = EnvGuard::unset("AUGMENTAGENT_DB");
        let opts = ask_opts(wiki.path().to_path_buf(), repo.path().to_path_buf());

        let (key, value) = opts
            .env
            .iter()
            .find(|(k, _)| k == "AUGMENTAGENT_DB")
            .expect("AUGMENTAGENT_DB env var present");
        assert_eq!(key, "AUGMENTAGENT_DB");
        let path = std::path::Path::new(value);
        assert!(path.is_absolute(), "db path must be absolute: {value}");
        assert_eq!(path.file_name().unwrap(), "data.db");
    }

    #[test]
    fn ask_opts_honors_augmentagent_db_override() {
        let repo = tempfile::tempdir().expect("repo tmpdir");
        let wiki = tempfile::tempdir().expect("wiki tmpdir");
        let override_dir = tempfile::tempdir().expect("override tmpdir");
        let override_db = override_dir.path().join("custom.db");
        std::fs::write(&override_db, b"").unwrap();

        let _guard = EnvGuard::set("AUGMENTAGENT_DB", override_db.to_str().unwrap());
        let opts = ask_opts(wiki.path().to_path_buf(), repo.path().to_path_buf());
        let value = opts
            .env
            .iter()
            .find(|(k, _)| k == "AUGMENTAGENT_DB")
            .map(|(_, v)| v.clone())
            .expect("AUGMENTAGENT_DB env present");
        assert!(std::path::Path::new(&value).is_absolute());
        assert_eq!(
            std::path::Path::new(&value).file_name().unwrap(),
            "custom.db"
        );
    }

    #[test]
    fn ask_opts_includes_invoice_read_and_config_tools() {
        let repo = tempfile::tempdir().expect("repo tmpdir");
        let wiki = tempfile::tempdir().expect("wiki tmpdir");
        std::fs::write(repo.path().join("data.db"), b"").unwrap();
        let _guard = EnvGuard::unset("AUGMENTAGENT_DB");
        let opts = ask_opts(wiki.path().to_path_buf(), repo.path().to_path_buf());

        let joined = opts.allowed_tools.join("\n");
        for needle in [
            "invoice status)",
            "invoice draft *)",
            "invoice list-accounts)",
            "invoice set-recipient *)",
            "invoice set-entity *)",
            "invoice set-auto-draft *)",
        ] {
            assert!(
                joined.contains(needle),
                "expected allowed_tools to contain {needle}; got:\n{joined}"
            );
        }
    }

    /// Load-bearing safety invariant: the LLM must never be able to invoke
    /// the real-send path. Only the Discord Approve button can call `run`.
    #[test]
    fn ask_opts_excludes_invoice_run() {
        let repo = tempfile::tempdir().expect("repo tmpdir");
        let wiki = tempfile::tempdir().expect("wiki tmpdir");
        std::fs::write(repo.path().join("data.db"), b"").unwrap();
        let _guard = EnvGuard::unset("AUGMENTAGENT_DB");
        let opts = ask_opts(wiki.path().to_path_buf(), repo.path().to_path_buf());

        for entry in &opts.allowed_tools {
            assert!(
                !entry.contains("invoice run"),
                "allowed_tools must NEVER expose `invoice run` to the LLM: {entry}"
            );
        }
    }

    /// Serialize env-var mutations across the two tests so they don't race.
    /// (`cargo test` runs tests in parallel by default; both touch the same
    /// process-wide `AUGMENTAGENT_DB` var.)
    struct EnvGuard {
        key: &'static str,
        prior: Option<String>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl EnvGuard {
        fn lock() -> std::sync::MutexGuard<'static, ()> {
            static M: std::sync::Mutex<()> = std::sync::Mutex::new(());
            M.lock().unwrap_or_else(|e| e.into_inner())
        }
        fn unset(key: &'static str) -> Self {
            let lock = Self::lock();
            let prior = std::env::var(key).ok();
            std::env::remove_var(key);
            Self {
                key,
                prior,
                _lock: lock,
            }
        }
        fn set(key: &'static str, value: &str) -> Self {
            let lock = Self::lock();
            let prior = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self {
                key,
                prior,
                _lock: lock,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prior {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

/// Preset for the one-shot `resume ingest` CLI. Opus quality for a single-run
/// seeding pass. Full wiki R/W/E with cwd pinned so writes cannot escape.
pub fn resume_opts(wiki_root: PathBuf) -> ReasonerOpts {
    ReasonerOpts {
        system_prompt: include_str!("../../../schema/resume-ingest.md").to_string(),
        model: None, // Opus — seeding the wiki is high-leverage and one-shot
        allowed_tools: vec![
            "Read".into(),
            "Grep".into(),
            "Glob".into(),
            "Write".into(),
            "Edit".into(),
        ],
        add_dirs: vec![wiki_root.clone()],
        permission_mode: "acceptEdits".into(),
        cwd: Some(wiki_root),
        env: Vec::new(),
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StreamEvent {
    Assistant {
        message: AssistantMessage,
    },
    Result {
        #[serde(default)]
        result: Option<String>,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
struct AssistantMessage {
    #[serde(default)]
    content: Vec<ContentBlock>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlock {
    Text {
        text: String,
    },
    #[serde(other)]
    Other,
}
