//! Owner-only enrollment and authenticated Mac event transport. Not an agent tool.
use anyhow::{Context, Result};
use augmentagent_store::{imessage_owner::OwnerEvent, Store};
use clap::Subcommand;
use serde::Deserialize;
use std::io::Read;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub(crate) struct RunningTurn {
    pub conversation: String,
    pub cancel: CancellationToken,
}
pub(crate) struct RunningGuard<'a> {
    registry: &'a dashmap::DashMap<String, Arc<RunningTurn>>,
    key: String,
    record: Arc<RunningTurn>,
}
impl Drop for RunningGuard<'_> {
    fn drop(&mut self) {
        self.registry.remove_if(&self.key, |_, r| {
            Arc::ptr_eq(r, &self.record) && Arc::strong_count(r) == 2
        });
    }
}
pub(crate) fn conversation_key(ctx: &augmentagent_approval_discord::AuditCtx) -> String {
    format!(
        "{:?}:{}",
        ctx.guild_id,
        ctx.channel_id.map(|c| c.get()).unwrap_or(0)
    )
}
pub(crate) fn begin_turn<'a>(
    query: &'a crate::WikiQuerier,
    ctx: &augmentagent_approval_discord::AuditCtx,
) -> (CancellationToken, RunningGuard<'a>) {
    let record = query
        .running_text_turns
        .entry(ctx.session_id.clone())
        .or_insert_with(|| {
            Arc::new(RunningTurn {
                conversation: conversation_key(ctx),
                cancel: CancellationToken::new(),
            })
        })
        .clone();
    let cancel = record.cancel.clone();
    (
        cancel,
        RunningGuard {
            registry: &query.running_text_turns,
            key: ctx.session_id.clone(),
            record,
        },
    )
}

fn cancel_queued_chats(store: &Store) -> augmentagent_store::StoreResult<usize> {
    use augmentagent_approval_discord::imessage_owner_control::{
        immediate_command, parse, Control,
    };
    store.cancel_queued_imessage_owner_chats(|event| {
        event.kind == "text"
            && matches!(
                parse(&event.body, event.target_guid.as_deref()),
                Control::Chat
            )
            && !immediate_command(&event.body)
    })
}

/// Uses the same model, scheduler, journal and process services as Discord.
pub(crate) async fn owner_command(
    query: &crate::WikiQuerier,
    ctx: &augmentagent_approval_discord::AuditCtx,
    text: &str,
) -> Option<String> {
    use augmentagent_approval_discord::{JournalCmd, QueryHandler};
    if !ctx.owner_authorized {
        return Some("Only the configured owner can use these controls.".into());
    }
    let text = text.trim();
    let lower = text.to_ascii_lowercase();
    let store = query.conversation_store.as_deref();
    let channel = ctx.channel_id.map(|c| c.get()).unwrap_or(0);
    if lower == "model" || lower.starts_with("model ") {
        return query
            .model_command_in_guild(ctx.guild_id, channel, text)
            .await;
    }
    if lower == "status" {
        let running = query
            .running_text_turns
            .iter()
            .filter(|r| r.conversation == conversation_key(ctx))
            .count();
        let health = store.map(|s| s.imessage_owner_status(chrono::Utc::now().timestamp_millis()));
        let model = query.selected_model(channel).await;
        return Some(format!(
            "Running requests: {running}\nModel: {}\nOwner text status: {}",
            model.ok().flatten().unwrap_or_else(|| "default".into()),
            match health {
                Some(Ok(v)) => v.to_string(),
                Some(Err(e)) => e.to_string(),
                None => "storage unavailable".into(),
            }
        ));
    }
    if lower == "cancel" || lower == "cancel all" {
        let key = conversation_key(ctx);
        let turns: Vec<_> = query
            .running_text_turns
            .iter()
            .filter(|r| r.conversation == key)
            .map(|r| r.cancel.clone())
            .collect();
        for token in &turns {
            token.cancel();
        }
        let dropped = if lower == "cancel all" {
            store.map(cancel_queued_chats).transpose()
        } else {
            Ok(None)
        };
        return Some(match dropped {Err(e)=>format!("Stop requested for {} running request(s); queued cancellation failed: {e}",turns.len()),
            Ok(n)=>format!("Stop requested for {} running request(s). {} queued request(s) cancelled. Approval actions and scheduled loops keep their own controls.",turns.len(),n.unwrap_or(0))});
    }
    if lower == "reset" || lower == "new" {
        if query
            .running_text_turns
            .iter()
            .any(|r| r.conversation == conversation_key(ctx))
        {
            return Some(
                "A request is running. Send cancel, wait for it to stop, then reset.".into(),
            );
        }
        return Some(match (store, ctx.guild_id) {
            (Some(s), Some(guild)) => match s
                .reset_imessage_owner_conversation(&guild.to_string(), &channel.to_string())
            {
                Ok(()) => {
                    "Conversation reset. Earlier actions remain consumed; nothing is replayed."
                        .into()
                }
                Err(e) => format!("Reset failed: {e}"),
            },
            _ => "Conversation storage unavailable.".into(),
        });
    }
    if lower == "loop" || lower.starts_with("loop ") {
        let owner = store
            .and_then(|s| s.imessage_owner_context().ok().flatten())
            .map(|c| c.owner_id.to_string())
            .unwrap_or_default();
        let parser = crate::LoopReasonerParser {
            reasoner: query.reasoner.clone(),
        };
        let model = match query.selected_model(channel).await {
            Ok(v) => v,
            Err(e) => return Some(format!("Model selection unavailable: {e}")),
        };
        return Some(
            augmentagent_approval_discord::handle_loop_command_with_model(
                store,
                Some(&parser),
                &owner,
                &channel.to_string(),
                text,
                model.as_deref(),
            )
            .await,
        );
    }
    if lower == "!loops"
        || lower.starts_with("!loops ")
        || lower == "!processes"
        || lower.starts_with("!processes ")
    {
        let command = text.replacen("!processes", "!loops", 1);
        return Some(augmentagent_approval_discord::handle_process_command(&command, true).await);
    }
    if let Some(command) = augmentagent_approval_discord::parse_journal_command(text) {
        if matches!(command, JournalCmd::Usage) {
            return Some(augmentagent_approval_discord::JOURNAL_USAGE.into());
        }
        let Some(ops) =
            crate::journal_ops_for_wiki(Some(query.wiki_root.clone()), &query.reasoner).await
        else {
            return Some(augmentagent_approval_discord::JOURNAL_NOT_CONFIGURED.into());
        };
        return Some(match command {
            JournalCmd::Text(body) => ops.save_text(None, &body).await.unwrap_or_else(|e| e),
            JournalCmd::Done { title } => {
                let mut history = store
                    .and_then(|s| s.imessage_owner_chat_history(&channel.to_string()).ok())
                    .unwrap_or_default();
                if let (Some(http), Some(origin)) = (
                    ctx.http.as_ref(),
                    store.and_then(|s| s.imessage_owner_context().ok().flatten()),
                ) {
                    if let Ok(h) = augmentagent_approval_discord::fetch_owner_history(
                        http,
                        serenity::all::ChannelId::new(channel),
                        None,
                        Some(serenity::all::UserId::new(origin.bot_id)),
                        Some(serenity::all::UserId::new(origin.owner_id)),
                    )
                    .await
                    {
                        history.push_str(&h);
                    }
                }
                if history.trim().is_empty() {
                    "No recent conversation to compose from. Use !journal <text>.".into()
                } else {
                    ops.compose_and_save(&history, title)
                        .await
                        .unwrap_or_else(|e| e)
                }
            }
            JournalCmd::Usage => unreachable!(),
        });
    }
    if lower.starts_with("!voice") {
        return Some("Live voice and audio use Discord or the existing phone service. This iMessage connection supports text and native reactions.".into());
    }
    None
}

#[derive(Debug, Subcommand)]
pub enum Op {
    /// Inspect the current private owner binding and native capture cursor.
    Status,
    /// Read existing owner receipts/results; no mutations or replay.
    History {
        #[arg(long)]
        guid: Option<String>,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Enroll an exact private native conversation, starting after current history.
    Bind {
        #[arg(long)]
        chat_guid: String,
        #[arg(long)]
        owner_handle: String,
        #[arg(long)]
        self_chat: bool,
        #[arg(long)]
        current_rowid: i64,
    },
    /// Stop new owner deliveries and input dispatch. In-flight sends may arrive.
    Disable,
    /// Retire one quarantined/uncertain input without replay or delivery recovery.
    DiscardInput { guid: String },
    /// Accept a bounded structured native event batch on stdin (Mac SSH transport).
    Ingest,
    /// Report a fixed, redacted Mac capture diagnostic.
    CaptureError {
        #[arg(long)]
        message: String,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Batch {
    revision: i64,
    watermark: i64,
    events: Vec<OwnerEvent>,
}

pub fn run(store: &Store, op: &Op) -> Result<()> {
    match op {
        Op::DiscardInput { guid } => println!(
            "Retired {} input(s). Outbound deliveries are unchanged.",
            store.discard_imessage_owner_input(guid)?
        ),
        Op::History {
            guid,
            offset,
            limit,
        } => println!(
            "{}",
            store.imessage_owner_history(guid.as_deref(), *offset, *limit)?
        ),
        Op::Status => println!(
            "{}",
            serde_json::json!({"version":1,"binding":store.imessage_owner_binding()?,
                "health":store.imessage_owner_status(chrono::Utc::now().timestamp_millis())?})
        ),
        Op::Bind {
            chat_guid,
            owner_handle,
            self_chat,
            current_rowid,
        } => {
            let b =
                store.bind_imessage_owner(chat_guid, owner_handle, *self_chat, *current_rowid)?;
            println!("{}", serde_json::json!({"version":1,"binding":b}));
        }
        Op::Disable => {
            store.disable_imessage_owner()?;
            println!("Owner iMessage surface disabled. Previously claimed sends may still arrive.");
        }
        Op::CaptureError { message } => {
            store.report_imessage_owner_capture_error(message)?;
            println!("capture diagnostic recorded");
        }
        Op::Ingest => {
            let mut data = String::new();
            std::io::stdin().take(4_000_001).read_to_string(&mut data)?;
            anyhow::ensure!(data.len() <= 4_000_000, "owner event batch too large");
            let b: Batch = serde_json::from_str(&data).context("invalid owner event batch")?;
            let accepted = store.accept_imessage_owner_events(
                b.revision,
                b.watermark,
                &b.events,
                chrono::Utc::now().timestamp_millis(),
            )?;
            println!(
                "{}",
                serde_json::json!({"version":1,"accepted":accepted,"watermark":b.watermark})
            );
        }
    }
    Ok(())
}

/// Started only by serve, never by a one-shot compose running beside it.
pub fn start_runtime(
    surface: std::sync::Arc<augmentagent_approval_discord::imessage_owner::OwnerSurface>,
    handler: std::sync::Arc<dyn augmentagent_approval_discord::ApprovalActionHandler>,
    query: std::sync::Arc<crate::WikiQuerier>,
) {
    use std::sync::Arc;
    let store = surface.store.clone();
    if let Err(error) = store.interrupt_imessage_owner_events(chrono::Utc::now().timestamp_millis())
    {
        tracing::error!("owner iMessage recovery failed: {error}");
        return;
    }
    let Ok(token) = std::env::var("DISCORD_BOT_TOKEN") else {
        return;
    };
    let http = Arc::new(serenity::http::Http::new(&token));
    let mirror = surface.clone();
    let mirror_http = http.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        let mut offset = 0usize;
        let mut tick = 0usize;
        let mut last_problem = None;
        loop {
            interval.tick().await;
            match ensure_context(&mirror.store, &mirror_http).await {
                Ok(Some(_)) => {}
                Ok(None) => continue,
                Err(error) => {
                    let problem = error.to_string();
                    let _ = mirror.store.report_imessage_owner_runtime(
                        false,
                        Some(&problem),
                        chrono::Utc::now().timestamp_millis(),
                    );
                    if last_problem.as_ref() != Some(&problem) {
                        tracing::warn!("owner iMessage context unavailable: {problem}");
                        last_problem = Some(problem);
                    }
                    continue;
                }
            }
            if last_problem.take().is_some() {
                let _ = mirror.store.report_imessage_owner_runtime(
                    false,
                    None,
                    chrono::Utc::now().timestamp_millis(),
                );
            }
            tick = tick.wrapping_add(1);
            if tick % 3 != 1 {
                continue;
            }
            let result = mirror.catch_up(&mirror_http, offset).await;
            let error = result.as_ref().err().map(ToString::to_string);
            let _ = mirror.store.report_imessage_owner_runtime(
                true,
                error.as_deref(),
                chrono::Utc::now().timestamp_millis(),
            );
            if let Some(error) = error {
                tracing::warn!("owner iMessage mirror catch-up incomplete: {error}");
            }
            offset = offset.wrapping_add(5);
        }
    });
    // One conversational lane; four bounded deterministic control lanes.
    // A slow agent answer or Revise cannot block another draft's approval.
    for lane in 0..5 {
        let surface = surface.clone();
        let handler = handler.clone();
        let query = query.clone();
        let http = http.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
            loop {
                interval.tick().await;
                if !surface.active() {
                    continue;
                }
                let Ok(Some(origin)) = surface.store.imessage_owner_context() else {
                    continue;
                };
                let context = augmentagent_approval_discord::AuditCtx {
                    session_id: String::new(),
                    guild_id: Some(origin.guild_id),
                    channel_id: Some(serenity::all::ChannelId::new(origin.channel_id)),
                    http: Some(http.clone()),
                    owner_authorized: true,
                };
                let result = if lane == 0 {
                    surface
                        .dispatch_chat_one(handler.as_ref(), query.as_ref(), context)
                        .await
                } else {
                    surface
                        .dispatch_control_one(handler.as_ref(), query.as_ref(), context)
                        .await
                };
                if let Err(error) = result {
                    let _ = surface.store.report_imessage_owner_runtime(
                        false,
                        Some(&error.to_string()),
                        chrono::Utc::now().timestamp_millis(),
                    );
                    tracing::warn!(
                        "owner iMessage dispatcher stopped at uncertain work: {error:#}"
                    );
                }
            }
        });
    }
}

async fn ensure_context(
    store: &Store,
    http: &serenity::http::Http,
) -> Result<Option<augmentagent_store::imessage_owner::OwnerContext>> {
    let Some(binding) = store.imessage_owner_binding()?.filter(|b| b.enabled) else {
        return Ok(None);
    };
    let token =
        std::env::var("DISCORD_BOT_TOKEN").context("Discord owner identity is not configured")?;
    let owner: u64 = std::env::var("DISCORD_ALLOWED_USER_ID")?.parse()?;
    let approval: u64 = std::env::var("DISCORD_CHANNEL_ID")?.parse()?;
    let channel: u64 = std::env::var("DISCORD_QUERY_CHANNEL_ID")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(approval);
    let digest = augmentagent_approval_discord::imessage_owner::origin_digest(
        &token, owner, channel, approval,
    );
    if let Some(existing) = store.imessage_owner_context()? {
        anyhow::ensure!(
            existing.config_digest == digest,
            "Discord owner configuration changed; disable and re-enroll the owner text surface"
        );
        return Ok(Some(existing));
    }
    let user = http.get_current_user().await?;
    let native = serenity::all::ChannelId::new(channel)
        .to_channel(&http)
        .await?;
    let guild = match native {
        serenity::all::Channel::Guild(c) => c.guild_id.get(),
        _ => anyhow::bail!("owner text context needs the configured owner guild conversation"),
    };
    // An owner setting alone is not evidence that this token reaches that guild.
    serenity::all::GuildId::new(guild)
        .member(&http, serenity::all::UserId::new(owner))
        .await?;
    let context = augmentagent_store::imessage_owner::OwnerContext {
        revision: binding.revision,
        config_digest: digest,
        guild_id: guild,
        channel_id: channel,
        approval_channel_id: approval,
        owner_id: owner,
        bot_id: user.id.get(),
    };
    store.configure_imessage_owner_context(&context)?;
    Ok(Some(context))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn cancellation_reaches_the_existing_turn_token_and_preserves_approval_input() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(":memory:").unwrap());
        let query = crate::wiki_querier(
            crate::build_reasoner(),
            root.path().to_owned(),
            root.path().to_owned(),
            store.clone(),
            false,
        );
        let mut ctx = augmentagent_approval_discord::AuditCtx::empty();
        ctx.guild_id = Some(20);
        ctx.channel_id = Some(serenity::all::ChannelId::new(10));
        ctx.owner_authorized = true;
        ctx.session_id = "active-native-turn".into();
        let (token, guard) = begin_turn(&query, &ctx);
        assert!(!token.is_cancelled());
        let b = store
            .bind_imessage_owner("owner-chat", "owner@example.invalid", false, 0)
            .unwrap();
        for (row, body) in [
            (1, "approve abcdef01"),
            (2, "research this"),
            (3, "loop list"),
        ] {
            let event = OwnerEvent {
                binding_revision: b.revision,
                rowid: row,
                created_at_ms: chrono::Utc::now().timestamp_millis() + 1000,
                guid: format!("input-{row}"),
                chat_guid: b.chat_guid.clone(),
                sender: b.owner_handle.clone(),
                from_me: false,
                kind: "text".into(),
                target_guid: None,
                body: body.into(),
            };
            store
                .accept_imessage_owner_events(b.revision, row, &[event], 1)
                .unwrap();
        }
        let response = owner_command(&query, &ctx, "cancel all").await.unwrap();
        assert!(response.contains("1 running"));
        assert!(token.is_cancelled());
        let rows = store.imessage_owner_history(None, 0, 20).unwrap();
        let records = rows["records"].as_array().unwrap();
        assert_eq!(
            records.iter().find(|r| r["guid"] == "input-1").unwrap()["state"],
            "accepted"
        );
        assert_eq!(
            records.iter().find(|r| r["guid"] == "input-2").unwrap()["state"],
            "ignored"
        );
        assert_eq!(
            records.iter().find(|r| r["guid"] == "input-3").unwrap()["state"],
            "accepted"
        );
        drop(guard);
        assert!(query.running_text_turns.is_empty());
    }
}
