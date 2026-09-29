//! #1288 — one owner conversation turn on any surface, with native session
//! continuity: the transport-neutral core of what Discord's conversation
//! path (#1220) did inline, now shared by Discord and Slack.
//!
//! [`run_surface_turn`]:
//!
//! 1. loads the conversation's native binding (`surface_conversations`) and
//!    refuses to continue a session marked uncertain;
//! 2. picks the provider: the bound one, else the selection (default
//!    Claude). A selection other than Claude or Codex keeps the legacy
//!    history-in-prompt route, with no native session and no claim;
//! 3. **persists the turn claim before the agent runs** (`claim_surface_turn`),
//!    so a redelivered event or a restarted daemon can never submit the same
//!    turn twice;
//! 4. runs the caller's `answer` future inside [`native_session::CURRENT`],
//!    which the Claude and Codex adapters join to create or resume the
//!    provider's own session;
//! 5. binds a new conversation to the session the provider reported, marks
//!    it uncertain if the provider left mid-turn, and records the outcome.
//!
//! **Cancellation** (Slack's `cancel`): when `cancel` fires, the answer
//! future is dropped, which tears the provider's process tree down (the
//! provider supervisor reaps every descendant before its guard returns).
//! The session the turn created is still bound, and the claim is resolved
//! as cancelled ([`Store::resolve_surface_turn`]): never re-run, and the
//! conversation's next turn resumes the same native session. Discord never
//! passes a token, so its behaviour is unchanged.
//!
//! Keys: the conversation is the transport's stable
//! [`SurfaceConversationRef`] (for Discord: account = guild, conversation =
//! channel, no thread, which the store maps onto the original Discord
//! tables); the turn ID doubles as the audit and handoff identity and must
//! be globally namespaced by the caller.
//!
//! [`TURN_ENV`] carries per-turn environment for the agent process (Slack's
//! inbound file directory for the read-only scope-guard carve-out); query
//! handlers append [`turn_env`] to their spawn environment.

use std::future::Future;
use std::sync::Arc;

use augmentagent_store::{NativeConversation, Store, SurfaceTurnRef, SurfaceTurnResolution};
use tokio_util::sync::CancellationToken;

use crate::native_session::{NativeSession, CURRENT};
use crate::providers::ProviderKind;

tokio::task_local! {
    /// Extra environment for the agent spawned inside this turn.
    pub static TURN_ENV: Vec<(String, String)>;
}

/// The current turn's extra environment; empty outside a [`TURN_ENV`] scope.
pub fn turn_env() -> Vec<(String, String)> {
    TURN_ENV.try_with(Clone::clone).unwrap_or_default()
}

/// One turn to run.
#[derive(Debug, Clone, Copy)]
pub struct SurfaceTurnRequest<'a> {
    /// Conversation (the session key) and globally unique turn ID.
    pub turn: &'a SurfaceTurnRef,
    /// Earlier transcript, sent only when a new session is created (or on
    /// the legacy route). Empty when the transport has none.
    pub history: &'a str,
    /// The owner's message, attachments included.
    pub current: &'a str,
    /// Recorded on a new binding: the agent's working directory.
    pub cwd: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SurfaceTurnOutcome {
    Answered(String),
    /// Stopped by the caller's token; the claim is resolved as cancelled.
    Cancelled,
}

fn legacy_prompt(history: &str, current: &str) -> String {
    if history.is_empty() {
        current.to_string()
    } else {
        format!("{history}\n\nuser's current message:\n{current}")
    }
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

/// Run one turn. `select` is read after the binding (the conversation's
/// model selection); `answer` gets the prompt to send and runs the shared
/// agent (tools, wiki, skills, audit, fallback). See the module docs.
pub async fn run_surface_turn<S, F, Fut>(
    store: &Store,
    request: SurfaceTurnRequest<'_>,
    select: S,
    cancel: Option<&CancellationToken>,
    answer: F,
) -> anyhow::Result<SurfaceTurnOutcome>
where
    S: FnOnce() -> anyhow::Result<Option<ProviderKind>>,
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = anyhow::Result<String>>,
{
    let turn = request.turn;
    let chat = turn.conversation();
    let discord = chat.account().platform().as_str() == "discord";
    let binding = store.surface_conversation(chat)?;
    if binding.as_ref().is_some_and(|item| item.uncertain) {
        anyhow::bail!("Native conversation has an uncertain turn; inspect it before continuing");
    }
    let selection = select()?;
    let provider = if let Some(item) = &binding {
        let bound = match item.provider.as_str() {
            "claude" => ProviderKind::Claude,
            "codex" => ProviderKind::Codex,
            _ => anyhow::bail!("Unsupported bound native provider"),
        };
        if selection.is_some_and(|selected| selected != bound) {
            anyhow::bail!("Model selection conflicts with bound native session; explicitly end or migrate the conversation");
        }
        bound
    } else {
        let selected = selection.unwrap_or(ProviderKind::Claude);
        if !matches!(selected, ProviderKind::Claude | ProviderKind::Codex) {
            // Profiles without a native session keep their existing text
            // route until the owner explicitly selects Claude or Codex.
            return Ok(SurfaceTurnOutcome::Answered(
                answer(legacy_prompt(request.history, request.current)).await?,
            ));
        }
        selected
    };
    let session = NativeSession::from_id(
        provider,
        binding.as_ref().map(|item| item.native_session_id.clone()),
    )?;
    let prompt = if binding.is_some() {
        request.current.to_string()
    } else {
        legacy_prompt(request.history, request.current)
    };
    // Persist the claim before the CLI can run tools. If the daemon dies
    // before recording a result or native ID, restart must not silently
    // submit this turn a second time.
    store.claim_surface_turn(turn)?;
    let handler_dispatched_at_ms = now_ms();
    let (turn_id, account, conversation) = (
        turn.turn_id(),
        chat.account().account_id(),
        chat.conversation_id(),
    );
    if discord {
        tracing::info!(turn_id = %turn_id, guild = %account, channel = %conversation,
            handler_dispatched_at_ms, "Discord native turn handler dispatched");
    } else {
        tracing::info!(turn_id = %turn_id, platform = chat.account().platform().as_str(),
            account = %account, conversation = %conversation, thread = ?chat.thread_id(),
            handler_dispatched_at_ms, "surface native turn handler dispatched");
    }
    let run = CURRENT.scope(Arc::clone(&session), answer(prompt));
    let answered = match cancel {
        Some(token) => tokio::select! {
            biased;
            _ = token.cancelled() => None,
            result = run => Some(result),
        },
        None => Some(run.await),
    };
    let answer_completed_at_ms = now_ms();
    if discord {
        tracing::info!(turn_id = %turn_id, guild = %account, channel = %conversation,
            ?handler_dispatched_at_ms, ?answer_completed_at_ms,
            native_submitted_at_ms = ?session.native_submitted_at_ms(),
            first_text_output_at_ms = ?session.first_text_output_at_ms(),
            "Discord native turn timing");
    } else {
        tracing::info!(turn_id = %turn_id, platform = chat.account().platform().as_str(),
            ?handler_dispatched_at_ms, ?answer_completed_at_ms, cancelled = answered.is_none(),
            native_submitted_at_ms = ?session.native_submitted_at_ms(),
            first_text_output_at_ms = ?session.first_text_output_at_ms(),
            "surface native turn timing");
    }
    let Some(answer) = answered else {
        // Cancelled: the provider future is gone and its process tree with
        // it. Keep the session so the thread continues it, and resolve the
        // claim so it is never re-run but no longer blocks the thread.
        if let (Some(id), None) = (session.id(), &binding) {
            store.bind_surface_conversation(&NativeConversation {
                conversation: chat.clone(),
                provider: provider.name().to_string(),
                native_session_id: id,
                cwd: request.cwd.to_string(),
                uncertain: false,
            })?;
        }
        store.resolve_surface_turn(turn, SurfaceTurnResolution::Cancelled)?;
        return Ok(SurfaceTurnOutcome::Cancelled);
    };
    if let Some(id) = session.id() {
        if binding.is_none() {
            store.bind_surface_conversation(&NativeConversation {
                conversation: chat.clone(),
                provider: provider.name().to_string(),
                native_session_id: id,
                cwd: request.cwd.to_string(),
                uncertain: session.is_uncertain(),
            })?;
        } else if session.is_uncertain() {
            store.mark_surface_conversation_uncertain(chat)?;
        }
    }
    // A refusal before NativeSession::begin has no native side effects.
    // Preserve this turn ID as consumed, but let a later turn proceed.
    // A dropped lease marks the session uncertain and blocks replay.
    store.finish_surface_turn(turn, !session.is_uncertain())?;
    answer.map(SurfaceTurnOutcome::Answered)
}
