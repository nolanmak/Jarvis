//! Direct `claude` CLI reasoner.
//!
//! Phase 1 implementation: spawns the `claude` binary with `-p` and reads
//! the final assistant text from stream-json stdout. Authenticated via the
//! user's Claude Max subscription session (`claude login`), no API key.
//!
//! Phase 2 replaces this with `claudekernel::ClaudeSession` from dangercat;
//! the `Reasoner` trait boundary makes that swap mechanical.

use std::process::Stdio;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tracing::{debug, warn};

use crate::channel::Reasoner;

pub struct ClaudeCliReasoner {
    bin: String,
    model: Option<String>,
    extra_args: Vec<String>,
}

impl Default for ClaudeCliReasoner {
    fn default() -> Self {
        Self {
            bin: std::env::var("CLAUDE_CLI").unwrap_or_else(|_| "claude".into()),
            model: None,
            extra_args: Vec::new(),
        }
    }
}

impl ClaudeCliReasoner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }
}

#[async_trait]
impl Reasoner for ClaudeCliReasoner {
    async fn decide(&self, system_prompt: &str, user_message: &str) -> anyhow::Result<String> {
        let mut args: Vec<String> = vec![
            "-p".into(),
            "--output-format".into(),
            "stream-json".into(),
            "--verbose".into(),
            "--permission-mode".into(),
            "default".into(),
            "--allowedTools".into(),
            String::new(), // explicit empty allow-list: no tools
        ];
        if !system_prompt.trim().is_empty() {
            args.push("--append-system-prompt".into());
            args.push(system_prompt.to_string());
        }
        if let Some(m) = &self.model {
            args.push("--model".into());
            args.push(m.clone());
        }
        args.extend(self.extra_args.clone());

        let mut child = Command::new(&self.bin)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

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
                return Err(anyhow::anyhow!("claude exited non-zero: {status:?}: {stderr_buf}"));
            }
        }
        if final_text.is_empty() {
            return Err(anyhow::anyhow!("claude produced no assistant text"));
        }
        Ok(final_text)
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StreamEvent {
    Assistant { message: AssistantMessage },
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
    Text { text: String },
    #[serde(other)]
    Other,
}
