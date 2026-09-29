//! #1294 — `augmentagent slack deliver`: operator/debug path that sends an
//! answer (Markdown text and optional files) into a Slack conversation
//! through the same pipeline the daemon will use: mrkdwn conversion,
//! splitting, the durable outbox (one idempotency key per part) and the Web
//! API with the installed app's bot token (`slack app install`, #1284).
//!
//! Re-running the same command is safe: the default turn ID is derived from
//! the workspace, conversation, text and files, so the parts are already in
//! the outbox and nothing is sent twice. Pass `--turn-id` to send the same
//! text again on purpose. It lives beside `slack app` rather than under it so
//! the app lifecycle module stays untouched.
//!
//! Tokens are never printed or logged. Exit status is 0 only when every part
//! is delivered; otherwise each open part is listed with a recovery hint.

use std::io::Read;
use std::path::PathBuf;

use anyhow::Result;
use augmentagent_channel_slack::app::{api_base_from, SlackAppStore, SLACK_API_BASE_ENV};
use augmentagent_channel_slack::delivery::{
    enqueue_answer, Answer, AnswerFile, DispatchOutcome, PlanOptions, SlackOutboxDispatcher,
    DEFAULT_PART_CHARS,
};
use augmentagent_channel_slack::surface::SlackWorkspace;
use augmentagent_channel_slack::transport::{HttpSlackWebApi, WebApiConfig};
use augmentagent_store::delivery::SendStatus;
use augmentagent_store::Store;
use clap::Args;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// Largest answer read from a file or stdin.
const MAX_TEXT_BYTES: u64 = 1024 * 1024;

#[derive(Args, Debug, Clone)]
pub struct DeliverArgs {
    /// Channel, group or DM ID to deliver into.
    #[arg(long)]
    pub channel: String,
    /// Parent message `ts`: deliver as replies in that thread.
    #[arg(long)]
    pub thread: Option<String>,
    /// Markdown answer to deliver.
    #[arg(long, value_name = "PATH", conflicts_with = "stdin")]
    pub text_file: Option<PathBuf>,
    /// Read the Markdown answer from stdin.
    #[arg(long)]
    pub stdin: bool,
    /// File to upload after the text (repeatable).
    #[arg(long = "file", value_name = "PATH")]
    pub files: Vec<PathBuf>,
    /// Workspace team id; defaults to the only installed workspace.
    #[arg(long)]
    pub team: Option<String>,
    /// Turn ID the part keys derive from. Default: a hash of the workspace,
    /// conversation, text and file paths, so a re-run sends nothing new.
    #[arg(long)]
    pub turn_id: Option<String>,
    /// Characters per message part.
    #[arg(long, default_value_t = DEFAULT_PART_CHARS)]
    pub part_chars: usize,
    #[arg(long, num_args = 0..=1, default_missing_value = "true", default_value_t = false, action = clap::ArgAction::Set)]
    pub json: bool,
}

struct Failure {
    code: &'static str,
    message: String,
    recovery: String,
}

impl Failure {
    fn new(code: &'static str, message: impl Into<String>, recovery: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            recovery: recovery.into(),
        }
    }
}

fn fail(json_out: bool, f: &Failure) -> ! {
    if json_out {
        println!(
            "{}",
            json!({"ok": false, "error": f.code, "message": f.message, "recovery": f.recovery})
        );
    } else {
        eprintln!("error: {}", f.message);
        eprintln!("recovery: {}", f.recovery);
    }
    std::process::exit(1);
}

fn read_text(args: &DeliverArgs) -> Result<String, Failure> {
    let bad = |m: String| {
        Failure::new(
            "text_input",
            m,
            "pass the answer with --text-file PATH or --stdin (UTF-8, at most 1 MiB)",
        )
    };
    let mut buf = String::new();
    if let Some(path) = &args.text_file {
        std::fs::File::open(path)
            .and_then(|f| f.take(MAX_TEXT_BYTES + 1).read_to_string(&mut buf))
            .map_err(|e| bad(format!("read {}: {e}", path.display())))?;
    } else if args.stdin {
        std::io::stdin()
            .lock()
            .take(MAX_TEXT_BYTES + 1)
            .read_to_string(&mut buf)
            .map_err(|e| bad(format!("read stdin: {e}")))?;
    } else if args.files.is_empty() {
        return Err(bad("nothing to deliver: no text and no --file".into()));
    }
    if buf.len() as u64 > MAX_TEXT_BYTES {
        return Err(bad("answer is larger than 1 MiB".into()));
    }
    Ok(buf)
}

fn default_turn_id(
    team: &str,
    channel: &str,
    thread: Option<&str>,
    text: &str,
    files: &[PathBuf],
) -> String {
    let mut h = Sha256::new();
    for part in [team, channel, thread.unwrap_or(""), text] {
        h.update((part.len() as u64).to_le_bytes());
        h.update(part.as_bytes());
    }
    for f in files {
        let p = f.to_string_lossy();
        h.update((p.len() as u64).to_le_bytes());
        h.update(p.as_bytes());
    }
    let digest = h.finalize();
    let hex: String = digest[..12].iter().map(|b| format!("{b:02x}")).collect();
    format!("cli-{hex}")
}

fn status_hint(status: SendStatus) -> &'static str {
    match status {
        SendStatus::Sent => "delivered",
        SendStatus::Queued | SendStatus::Sending => "not sent yet; run the same command again",
        SendStatus::Failed => "retrying after a temporary failure; run the same command again later",
        SendStatus::Reconcile => {
            "outcome unknown (the request may have reached Slack); check the conversation before sending again with a new --turn-id"
        }
        SendStatus::DeadLetter => {
            "failed permanently; fix the cause shown and send again with a new --turn-id"
        }
        SendStatus::Abandoned => "abandoned",
    }
}

/// Entry point for `augmentagent slack deliver`.
pub async fn run(args: &DeliverArgs, store: &Store) -> Result<()> {
    match run_inner(args, store).await {
        Ok(all_sent) => {
            if !all_sent {
                std::process::exit(1);
            }
            Ok(())
        }
        Err(f) => fail(args.json, &f),
    }
}

async fn run_inner(args: &DeliverArgs, store: &Store) -> Result<bool, Failure> {
    let app_err = |e: augmentagent_channel_slack::app::SlackAppError| {
        Failure::new(e.code(), e.to_string(), e.recovery())
    };
    let text = read_text(args)?;
    let creds = SlackAppStore::default_store();
    let team = creds.resolve_team(args.team.as_deref()).map_err(app_err)?;
    let installed = creds.load(&team).map_err(app_err)?.ok_or_else(|| {
        app_err(
            augmentagent_channel_slack::app::SlackAppError::NotInstalled {
                team_id: team.clone(),
            },
        )
    })?;

    let invalid = |m: String| {
        Failure::new(
            "invalid_target",
            m,
            "--channel is a Slack conversation ID (e.g. C0123ABCD) and --thread a message ts (e.g. 1700000000.000100)",
        )
    };
    let workspace = SlackWorkspace::new(&installed.team_id, None)
        .map_err(|e| invalid(format!("team id: {e}")))?;
    let conversation = workspace
        .conversation(&args.channel, args.thread.as_deref())
        .map_err(|e| invalid(e.to_string()))?;

    let files: Vec<AnswerFile> = args
        .files
        .iter()
        .map(|path| AnswerFile {
            path: path.clone(),
            filename: None,
            title: None,
            alt_text: None,
        })
        .collect();
    let turn_id = args.turn_id.clone().unwrap_or_else(|| {
        default_turn_id(
            &team,
            &args.channel,
            args.thread.as_deref(),
            &text,
            &args.files,
        )
    });
    let answer = Answer {
        turn_id: &turn_id,
        markdown: &text,
        files: &files,
    };
    let opts = PlanOptions {
        part_chars: args.part_chars,
        ..PlanOptions::default()
    };
    let now_ms = chrono::Utc::now().timestamp_millis();
    let enqueued = enqueue_answer(store, &conversation, &answer, &opts, now_ms).map_err(|e| {
        Failure::new(
            "plan",
            e.to_string(),
            "check the answer text and file paths",
        )
    })?;

    let raw_base = std::env::var(SLACK_API_BASE_ENV).ok();
    let base_url = api_base_from(raw_base.as_deref()).map_err(app_err)?;
    if raw_base.as_deref().is_some_and(|v| !v.trim().is_empty()) {
        tracing::warn!(api_base = %base_url, "{SLACK_API_BASE_ENV} overrides the Slack Web API base URL");
    }
    let api = HttpSlackWebApi::new(
        installed.bot_token.clone(),
        WebApiConfig {
            base_url,
            ..WebApiConfig::default()
        },
    )
    .map_err(|e| Failure::new("slack_api", e.to_string(), "check network access to Slack"))?;
    let dispatched = SlackOutboxDispatcher::new(store, &api, &workspace)
        .drain(now_ms)
        .await
        .map_err(|e| Failure::new("store", e.to_string(), "check AUGMENTAGENT_DB is writable"))?;

    let mut rows = Vec::new();
    let mut all_sent = true;
    for planned in &enqueued.sends {
        let row = store
            .outbound_send(planned.id)
            .map_err(|e| Failure::new("store", e.to_string(), "check AUGMENTAGENT_DB"))?
            .ok_or_else(|| Failure::new("store", "outbox row vanished", "run the command again"))?;
        let sent_now = dispatched
            .iter()
            .any(|d| d.id == planned.id && matches!(d.outcome, DispatchOutcome::Sent { .. }));
        all_sent &= row.status == SendStatus::Sent;
        rows.push(json!({
            "idempotency_key": row.idempotency_key,
            "operation": row.operation.as_str(),
            "status": row.status.as_str(),
            "sent_now": sent_now,
            "already_enqueued": planned.status.is_some(),
            "provider_message_id": row.provider_message_id,
            "attempts": row.attempts,
            "last_error": row.last_error,
            "hint": status_hint(row.status),
        }));
    }
    let sent_now = rows.iter().filter(|r| r["sent_now"] == json!(true)).count();
    if args.json {
        println!(
            "{}",
            json!({
                "ok": all_sent,
                "team_id": team,
                "channel": args.channel,
                "thread_ts": args.thread,
                "turn_id": turn_id,
                "parts": rows.len(),
                "newly_enqueued": enqueued.queued,
                "already_enqueued": enqueued.duplicates,
                "sent_now": sent_now,
                "sends": Value::Array(rows),
            })
        );
    } else {
        let place = match &args.thread {
            Some(ts) => format!("{} (thread {ts})", args.channel),
            None => args.channel.clone(),
        };
        println!(
            "Slack delivery to {place} in {team}: {} part(s), turn {turn_id}",
            rows.len()
        );
        if enqueued.duplicates > 0 {
            println!(
                "  {} part(s) were already in the outbox (same turn); they are not sent twice",
                enqueued.duplicates
            );
        }
        for r in &rows {
            println!(
                "  {:<42} {:<7} {:<11} {}{}",
                r["idempotency_key"].as_str().unwrap_or(""),
                r["operation"].as_str().unwrap_or(""),
                r["status"].as_str().unwrap_or(""),
                r["provider_message_id"].as_str().unwrap_or("-"),
                if r["sent_now"] == json!(true) {
                    "  (sent now)"
                } else {
                    ""
                },
            );
            if r["status"] != json!("sent") {
                println!("      {}", r["hint"].as_str().unwrap_or(""));
                if let Some(e) = r["last_error"].as_str() {
                    println!("      last error: {e}");
                }
            }
        }
        println!(
            "  sent now: {sent_now}; delivered in total: {}",
            rows.iter().filter(|r| r["status"] == json!("sent")).count()
        );
    }
    Ok(all_sent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_turn_id_is_stable_and_separates_its_inputs() {
        let a = default_turn_id("T00000001", "C00000001", None, "hello", &[]);
        assert_eq!(
            a,
            default_turn_id("T00000001", "C00000001", None, "hello", &[])
        );
        assert!(a.starts_with("cli-") && a.len() == 4 + 24);
        assert_ne!(
            a,
            default_turn_id("T00000001", "C00000001", Some("1.2"), "hello", &[])
        );
        assert_ne!(
            a,
            default_turn_id("T00000001", "C0000000", None, "1hello", &[])
        );
        assert_ne!(
            a,
            default_turn_id("T00000001", "C00000001", None, "hello", &["x.pdf".into()])
        );
    }
}
