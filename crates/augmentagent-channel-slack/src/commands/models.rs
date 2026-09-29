//! `model` on Slack: the same selection file and profiles as Discord's
//! `/model`, keyed by the transport-neutral conversation storage key
//! (`SurfaceConversationRef::storage_key`) instead of a Discord channel ID.
//!
//! Lookup for a conversation: its own choice, then (for a thread) its
//! channel's, then the daemon default, then the configured route. A
//! conversation bound to a native session keeps that session's provider;
//! switching it is refused (as on Discord) until `reset`.

use std::path::PathBuf;
use std::sync::Arc;

use augmentagent_channel_core::model_selection::SelectionStore;
use augmentagent_channel_core::providers::ProviderKind;
use augmentagent_store::{Store, SurfaceConversationRef};

use super::{usage_of, CommandContext, SlackCommandDeps};
use crate::harness::ProviderSelection;

/// Where a conversation's effective selection comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSource {
    Conversation,
    /// The channel a thread belongs to.
    Channel,
    DaemonDefault,
    /// No selection: the configured route decides at dispatch.
    Route,
}

impl ModelSource {
    fn label(self) -> &'static str {
        match self {
            Self::Conversation => "this conversation",
            Self::Channel => "this channel",
            Self::DaemonDefault => "daemon default",
            Self::Route => "existing route",
        }
    }
}

fn channel_of(conversation: &SurfaceConversationRef) -> Option<SurfaceConversationRef> {
    conversation.thread_id()?;
    SurfaceConversationRef::new(
        conversation.account().clone(),
        conversation.conversation_id(),
        None,
    )
    .ok()
}

/// The effective selection for `conversation` and where it comes from.
pub(super) fn resolve(
    store: &SelectionStore,
    conversation: &SurfaceConversationRef,
) -> anyhow::Result<(Option<ProviderKind>, ModelSource)> {
    let (own, source) = store.describe(&conversation.storage_key())?;
    if source == "conversation" {
        return Ok((own, ModelSource::Conversation));
    }
    if let Some(channel) = channel_of(conversation) {
        let (inherited, source) = store.describe(&channel.storage_key())?;
        if source == "conversation" {
            return Ok((inherited, ModelSource::Channel));
        }
    }
    Ok(match own {
        Some(kind) => (Some(kind), ModelSource::DaemonDefault),
        None => (None, ModelSource::Route),
    })
}

/// The harness's per-conversation selection over the selection file at
/// `path` (re-read on every turn, so a `model` command applies to the next
/// turn and survives a restart).
pub fn slack_selection(path: PathBuf) -> ProviderSelection {
    Arc::new(move |conversation| Ok(resolve(&SelectionStore::new(&path), conversation)?.0))
}

/// An explicit owner choice for this conversation or its channel (what a
/// new loop pins), never the daemon default.
pub(super) fn explicit_choice(
    deps: &SlackCommandDeps,
    conversation: &SurfaceConversationRef,
) -> Option<ProviderKind> {
    match resolve(&SelectionStore::new(&deps.selection_path), conversation) {
        Ok((kind, ModelSource::Conversation | ModelSource::Channel)) => kind,
        _ => None,
    }
}

/// One line for `status`.
pub(super) fn describe(deps: &SlackCommandDeps, conversation: &SurfaceConversationRef) -> String {
    match resolve(&SelectionStore::new(&deps.selection_path), conversation) {
        Ok((Some(kind), source)) => format!("{} ({})", kind.name(), source.label()),
        Ok((None, _)) => "existing route (no selection)".into(),
        Err(e) => format!("unavailable ({e})"),
    }
}

fn profile(name: &str) -> Option<ProviderKind> {
    match ProviderKind::parse(&name.to_ascii_lowercase()) {
        Some(
            kind @ (ProviderKind::Claude
            | ProviderKind::Qwen
            | ProviderKind::Glm
            | ProviderKind::Codex),
        ) => Some(kind),
        _ => None,
    }
}

const PROFILES: &str = "Profiles: claude (Claude Code), codex (Codex CLI), qwen (Runpod), glm \
     (Runpod). `model` shows this conversation's choice.";

pub(super) fn command(
    store: &Store,
    deps: &SlackCommandDeps,
    cx: &CommandContext<'_>,
    args: &str,
) -> String {
    let selection = SelectionStore::new(&deps.selection_path);
    let words: Vec<&str> = args.split_whitespace().collect();
    let lower: Vec<String> = words.iter().map(|w| w.to_ascii_lowercase()).collect();
    let lower: Vec<&str> = lower.iter().map(String::as_str).collect();
    match lower.as_slice() {
        [] | ["show"] | ["status"] => show(store, deps, &selection, cx.conversation),
        ["help"] => usage_of("model"),
        ["list"] => PROFILES.into(),
        ["reset"] => reset(&selection, Some(cx.conversation)),
        ["reset", "scope:default"] => reset(&selection, None),
        ["set", name] => set(store, deps, &selection, Some(cx.conversation), name),
        ["set", name, "scope:default"] => set(store, deps, &selection, None, name),
        [name] if !matches!(*name, "set") => {
            set(store, deps, &selection, Some(cx.conversation), name)
        }
        _ => usage_of("model"),
    }
}

fn show(
    store: &Store,
    deps: &SlackCommandDeps,
    selection: &SelectionStore,
    conversation: &SurfaceConversationRef,
) -> String {
    let session = match store.surface_conversation(conversation) {
        Ok(Some(b)) => format!(
            " Session: {} `{}` — new turns continue it.",
            b.provider, b.native_session_id
        ),
        Ok(None) => " No session yet: the next turn starts one with this model.".into(),
        Err(_) => String::new(),
    };
    match resolve(selection, conversation) {
        Ok((Some(kind), source)) => {
            let readiness = match (deps.model_ready)(kind) {
                Ok(()) => "ready".to_string(),
                Err(reason) => reason,
            };
            format!(
                "Model: {} ({}). Readiness: {readiness}.{session} Running requests keep their \
                 snapshot.",
                kind.name(),
                source.label()
            )
        }
        Ok((None, _)) => format!(
            "Model: existing route (no selection): chosen by the configured route at \
             dispatch.{session}"
        ),
        Err(e) => format!("Model status unavailable: {e}"),
    }
}

fn reset(selection: &SelectionStore, conversation: Option<&SurfaceConversationRef>) -> String {
    match selection.set(conversation.map(|c| c.storage_key()).as_deref(), None) {
        Ok(()) => format!(
            "Model selection reset for {}.",
            if conversation.is_some() {
                "this conversation"
            } else {
                "the daemon default"
            }
        ),
        Err(e) => format!("Model selection unchanged: {e}"),
    }
}

fn set(
    store: &Store,
    deps: &SlackCommandDeps,
    selection: &SelectionStore,
    conversation: Option<&SurfaceConversationRef>,
    name: &str,
) -> String {
    let Some(kind) = profile(name) else {
        return "Unknown model. Choose claude, codex, qwen or glm.".into();
    };
    if let Err(reason) = (deps.model_ready)(kind) {
        return format!("Model selection unchanged: {reason}");
    }
    if let Some(conversation) = conversation {
        match store.surface_conversation(conversation) {
            Ok(Some(bound)) if bound.provider != kind.name() => {
                return format!(
                    "This conversation is bound to native {} session `{}`. Switching models \
                     would start a different session, so the choice is unchanged. Send `reset` \
                     to start a new session here, then `model set {}`.",
                    bound.provider,
                    bound.native_session_id,
                    kind.name()
                )
            }
            Ok(_) => {}
            Err(e) => return format!("Model selection unavailable: {e}"),
        }
    }
    match selection.set(conversation.map(|c| c.storage_key()).as_deref(), Some(kind)) {
        Ok(()) => format!(
            "Model set to {} for {}. Running requests keep their current model.",
            kind.name(),
            if conversation.is_some() {
                "this conversation"
            } else {
                "the daemon default"
            }
        ),
        Err(e) => format!("Model selection unchanged: {e}"),
    }
}
