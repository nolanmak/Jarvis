//! #1293 — `augmentagent slack files fetch`: operator/debug path that runs
//! one Slack message's files through the inbound pipeline the daemon will
//! use (`augmentagent_channel_slack::inbound::prepare_inbound`) and prints
//! what the reasoner would get: accepted files, the owner-facing "skipped"
//! notice and the prompt. Files are downloaded with the installed app's bot
//! token (`slack app install`, #1284) into the private state dir and removed
//! before the command exits (also on Ctrl-C).
//!
//! The message is read with `conversations.replies` (which returns a single
//! message when `ts` has no replies), so it works for top-level messages and
//! thread replies alike. Tokens are never printed or logged.

use anyhow::Result;
use augmentagent_channel_slack::app::{api_base_from, SlackAppStore, SLACK_API_BASE_ENV};
use augmentagent_channel_slack::inbound::{default_inbound_root, prepare_inbound, InboundOptions};
use augmentagent_channel_slack::transport::event::file_refs;
use augmentagent_channel_slack::transport::web::{
    test_file_hosts_from, DownloadLimits, HistoryQuery, SlackWebApi, SLACK_TEST_FILE_HOSTS_ENV,
};
use augmentagent_channel_slack::transport::{HttpSlackWebApi, WebApiConfig};
use augmentagent_docs::inbound::{InboundKind, RejectReason};
use augmentagent_docs::DocKind;
use clap::{Args, Subcommand};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

/// Pages of a thread read while looking for `--ts`.
const MAX_PAGES: usize = 5;

#[derive(Subcommand, Debug, Clone)]
pub enum SlackFilesOp {
    /// Download one message's files through the inbound pipeline and print
    /// the resulting turn input (files are removed afterwards).
    Fetch(FetchArgs),
}

#[derive(Args, Debug, Clone)]
pub struct FetchArgs {
    /// Channel, group or DM ID the message is in.
    #[arg(long)]
    pub channel: String,
    /// The message `ts` (e.g. 1700000000.000100); a thread reply works too.
    #[arg(long)]
    pub ts: String,
    /// Workspace team id; defaults to the only installed workspace.
    #[arg(long)]
    pub team: Option<String>,
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

/// Entry point for `augmentagent slack files`.
pub async fn run(op: &SlackFilesOp) -> Result<()> {
    match op {
        SlackFilesOp::Fetch(args) => match fetch(args).await {
            Ok(v) => {
                if args.json {
                    println!("{v}");
                } else {
                    print_human(&v);
                }
                Ok(())
            }
            Err(f) => fail(args.json, &f),
        },
    }
}

fn kind_name(kind: InboundKind) -> &'static str {
    match kind {
        InboundKind::Image => "image",
        InboundKind::Text => "text",
        InboundKind::Doc(DocKind::Pdf) => "pdf",
        InboundKind::Doc(DocKind::Docx) => "docx",
        InboundKind::Doc(DocKind::Doc) => "doc",
    }
}

fn reason_name(reason: &RejectReason) -> &'static str {
    match reason {
        RejectReason::Oversize { .. } => "oversize",
        RejectReason::SecurityDenylist => "security",
        RejectReason::UnsupportedType { .. } => "unsupported",
        RejectReason::Unavailable(_) => "unavailable",
    }
}

async fn fetch(args: &FetchArgs) -> Result<Value, Failure> {
    let app_err = |e: augmentagent_channel_slack::app::SlackAppError| {
        Failure::new(e.code(), e.to_string(), e.recovery())
    };
    let valid_id = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric());
    let valid_ts = |s: &str| {
        s.split_once('.').is_some_and(|(a, b)| {
            !a.is_empty()
                && !b.is_empty()
                && (a.chars().chain(b.chars())).all(|c| c.is_ascii_digit())
        })
    };
    if !valid_id(&args.channel) || !valid_ts(&args.ts) {
        return Err(Failure::new(
            "invalid_target",
            "--channel must be a Slack conversation ID and --ts a message ts",
            "e.g. --channel D0123ABCD --ts 1700000000.000100 (\"Copy link\" on the message shows both)",
        ));
    }

    // Test-only loopback file hosts; refused unless loopback host:port.
    let raw_hosts = std::env::var(SLACK_TEST_FILE_HOSTS_ENV).ok();
    let test_hosts = test_file_hosts_from(raw_hosts.as_deref()).map_err(|m| {
        Failure::new(
            "invalid_test_file_host",
            m,
            format!("unset {SLACK_TEST_FILE_HOSTS_ENV}; it only accepts loopback host:port entries for local mock servers"),
        )
    })?;
    if !test_hosts.is_empty() {
        tracing::warn!(hosts = ?test_hosts, "{SLACK_TEST_FILE_HOSTS_ENV} adds loopback test file hosts");
    }

    let creds = SlackAppStore::default_store();
    let team = creds.resolve_team(args.team.as_deref()).map_err(app_err)?;
    let installed = creds.load(&team).map_err(app_err)?.ok_or_else(|| {
        app_err(
            augmentagent_channel_slack::app::SlackAppError::NotInstalled {
                team_id: team.clone(),
            },
        )
    })?;
    let raw_base = std::env::var(SLACK_API_BASE_ENV).ok();
    let base_url = api_base_from(raw_base.as_deref()).map_err(app_err)?;
    if raw_base.as_deref().is_some_and(|v| !v.trim().is_empty()) {
        tracing::warn!(api_base = %base_url, "{SLACK_API_BASE_ENV} overrides the Slack Web API base URL");
    }
    let mut limits = DownloadLimits::default();
    limits.allowed_hosts.extend(test_hosts);
    let cancel = CancellationToken::new();
    let api = HttpSlackWebApi::new(
        installed.bot_token.clone(),
        WebApiConfig {
            base_url,
            ..WebApiConfig::default()
        },
    )
    .map_err(|e| Failure::new("slack_api", e.to_string(), "check network access to Slack"))?
    .with_download_limits(limits)
    .scoped(cancel.clone());

    // Find the message (top-level or a reply) by its ts.
    let mut cursor = None;
    let mut found = None;
    for _ in 0..MAX_PAGES {
        let page = api
            .conversations_replies(HistoryQuery {
                channel: args.channel.clone(),
                thread_ts: Some(args.ts.clone()),
                oldest: None,
                limit: 200,
                cursor: cursor.clone(),
                include_all_metadata: false,
            })
            .await
            .map_err(|e| {
                Failure::new(
                    "slack_api",
                    format!("could not read the message: {e}"),
                    "check --channel/--ts and that the app is in the conversation (needs the *:history scopes)",
                )
            })?;
        found = page.messages.into_iter().find(|m| m.ts == args.ts);
        if found.is_some() || !page.has_more {
            break;
        }
        cursor = page.next_cursor;
    }
    let message = found.ok_or_else(|| {
        Failure::new(
            "message_not_found",
            format!("no message {} in {}", args.ts, args.channel),
            "copy the message link in Slack and pass its channel and ts",
        )
    })?;
    let files = file_refs(&message.raw);
    let text = message.text.clone().unwrap_or_default();

    let root = default_inbound_root().ok_or_else(|| {
        Failure::new(
            "storage",
            "cannot resolve the state dir (HOME unset)",
            "set HOME",
        )
    })?;
    let mut opts = InboundOptions::new(root.clone());
    let has_pdf = files.iter().any(|f| {
        augmentagent_docs::doc_kind_for(f.name.as_deref().unwrap_or(""), f.mimetype.as_deref())
            == Some(DocKind::Pdf)
    });
    if has_pdf {
        opts.ocr = augmentagent_docs::OcrClient::from_env();
    }

    // Ctrl-C cancels the downloads and conversions; the pipeline removes
    // whatever it had written.
    let on_signal = cancel.clone();
    let watcher = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            on_signal.cancel();
        }
    });
    let prepared = prepare_inbound(&api, &text, &files, &opts, &cancel).await;
    watcher.abort();
    let msg = prepared.map_err(|e| {
        Failure::new(
            "inbound",
            e.to_string(),
            "for storage errors, check that the state dir is a private directory you own",
        )
    })?;

    let accepted: Vec<Value> = msg
        .accepted
        .iter()
        .map(|a| {
            let text_file = msg.text_files.iter().find(|t| t.path == a.path);
            let preview = text_file.and_then(|t| {
                std::fs::read(&t.path).ok().map(|b| {
                    String::from_utf8_lossy(&b[..b.len().min(200)])
                        .trim()
                        .to_string()
                })
            });
            json!({
                "file_id": a.file_id,
                "name": a.original_name,
                "kind": kind_name(a.kind),
                "stored_as": a.path.file_name().map(|n| n.to_string_lossy().to_string()),
                "downloaded_bytes": a.downloaded_bytes,
                "truncated": text_file.map(|t| t.truncated),
                "note": text_file.and_then(|t| t.note.clone()),
                "preview": preview,
            })
        })
        .collect();
    let rejected: Vec<Value> = msg
        .rejected
        .iter()
        .map(|r| json!({"name": r.filename, "reason": reason_name(&r.reason)}))
        .collect();
    let v = json!({
        "ok": true,
        "channel": args.channel,
        "ts": args.ts,
        "text": msg.user_text,
        "files": files.len(),
        "starts_turn": msg.starts_turn(),
        "accepted": accepted,
        "rejected": rejected,
        "notice": msg.rejection_notice(),
        "prompt": msg.prompt,
        "inbound_root": root.display().to_string(),
    });
    let cleaned = msg.cleanup().is_ok();
    let mut v = v;
    v["cleaned_up"] = json!(cleaned);
    Ok(v)
}

fn print_human(v: &Value) {
    println!(
        "message {} in {}: {} file(s), text {:?}",
        v["ts"].as_str().unwrap_or_default(),
        v["channel"].as_str().unwrap_or_default(),
        v["files"],
        v["text"].as_str().unwrap_or_default()
    );
    for a in v["accepted"].as_array().into_iter().flatten() {
        println!(
            "  accepted  {:<6} {} ({} bytes){}",
            a["kind"].as_str().unwrap_or_default(),
            a["name"].as_str().unwrap_or_default(),
            a["downloaded_bytes"],
            a["preview"]
                .as_str()
                .map(|p| format!(" preview: {:?}", p.chars().take(60).collect::<String>()))
                .unwrap_or_default()
        );
    }
    for r in v["rejected"].as_array().into_iter().flatten() {
        println!(
            "  skipped   {:<6} {}",
            r["reason"].as_str().unwrap_or_default(),
            r["name"].as_str().unwrap_or_default()
        );
    }
    if let Some(notice) = v["notice"].as_str() {
        println!("owner notice: {notice}");
    }
    println!(
        "starts a turn: {}; files removed: {}",
        v["starts_turn"], v["cleaned_up"]
    );
    println!(
        "--- prompt ---\n{}",
        v["prompt"].as_str().unwrap_or_default()
    );
}
