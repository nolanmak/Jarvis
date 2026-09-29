//! #1288 — Slack owner turns through the shared agent harness.
//!
//! [`SlackConversationHarness`] is the [`SlackTurnHandler`] `serve` runs. It
//! is deliberately thin: everything the agent does comes from the same two
//! pieces Discord's conversation turns use.
//!
//! * **The agent** is a [`QueryHandler`] (in `serve`, the same `WikiQuerier`
//!   Discord answers with): the wiki-ask prompt and owner rules, the tool
//!   allowlist and scope guards, wiki and memory, skills, the tool audit log
//!   keyed by the turn ID, and provider fallback.
//! * **The turn** is [`run_surface_turn`], the transport-neutral native
//!   session runner Discord's conversation path now calls too: the
//!   conversation's native Claude/Codex session is created on the first turn
//!   and resumed on every follow-up, and the turn claim is persisted before
//!   the agent runs.
//!
//! Mapping: [`SlackTurn::session`] is the conversation (the DM, a DM thread,
//! or a channel thread; a top-level control-channel message starts its own
//! thread), [`SlackTurn::turn_id`] (`slack:<team>:<channel>:<ts>`) is the
//! claim, audit and handoff identity. Owner authority: a turn only reaches
//! this handler after `owner::admit` returned `Owner` for the bound
//! `(workspace, user)`, the Slack counterpart of Discord's explicit owner
//! allowlist match, so the agent runs with `owner_authorized: true`.
//!
//! Lifecycle:
//!
//! * **Redelivery / replay.** A turn whose claim already exists is never
//!   submitted again. A claim left pending by a daemon that died (or shut
//!   down) mid-turn is resolved as interrupted and the owner gets
//!   [`INTERRUPTED_REPLY`]; the conversation's next message resumes the same
//!   native session.
//! * **Cancel.** [`SlackTurn::cancel`] drops the running agent (and with it
//!   the provider's process tree), resolves the claim as cancelled and
//!   answers [`CANCELLED_REPLY`]; the thread's session continues.
//! * **Files.** The turn's inbound directory is handed to the agent process
//!   as [`SLACK_INBOUND_DIR_ENV`], which the wiki scope guard and the Codex
//!   bridge turn into a read-only allowance for exactly that directory.
//! * **Generated files.** `ATTACH:` markers in the answer are validated
//!   against the wiki root with Discord's rules and sent as Slack uploads.
//!
//! Not here: interactions and approval cards (#1289), owner commands such as
//! model selection (#1292), notifications (#1295).

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use augmentagent_approval_discord::attachments::extract_attach_markers;
use augmentagent_approval_discord::{AuditCtx, QueryHandler};
use augmentagent_channel_core::providers::ProviderKind;
use augmentagent_channel_core::surface_turn::{
    run_surface_turn, SurfaceTurnOutcome, SurfaceTurnRequest, TURN_ENV,
};
use augmentagent_store::{
    Store, SurfaceConversationRef, SurfaceTurnRef, SurfaceTurnResolution, SurfaceTurnStatus,
};
use tracing::{info, warn};

use crate::delivery::AnswerFile;
use crate::interactive::{SlackTurn, SlackTurnHandler, SlackTurnReply};
use crate::owner::OwnerInputSource;

pub use augmentagent_channel_core::codex_tools::SLACK_INBOUND_DIR_ENV;

/// After `cancel`: the turn was stopped and will not run again.
pub const CANCELLED_REPLY: &str = "Stopped. That request was cancelled and will not run again; \
     anything it already did stays done. Reply here to carry on in the same conversation.";

/// A turn a restart interrupted: it is not re-run, because its tools may
/// already have acted.
pub const INTERRUPTED_REPLY: &str = "Jarvis restarted while working on this, so it stopped \
     part-way and was not run again (it may already have done some of the work). Reply here \
     to carry on in the same conversation.";

/// A turn that finished just before a restart, whose answer was lost.
pub const FINISHED_BEFORE_RESTART_REPLY: &str = "This request finished just before Jarvis \
     restarted, but its answer was lost. Ask again if you still need it.";

/// The conversation's native session was left mid-turn by a provider
/// failure (#1220 semantics): it is not continued automatically.
pub const SESSION_UNCERTAIN_REPLY: &str = "An earlier request here stopped part-way inside the \
     agent, so I won't continue this conversation automatically. Start a new thread (or a new \
     top-level message in the control channel) to go on.";

/// Picks the provider for a conversation without a bound session: the
/// owner's model selection. `None` means the default (Claude).
pub type ProviderSelection =
    Arc<dyn Fn(&SurfaceConversationRef) -> anyhow::Result<Option<ProviderKind>> + Send + Sync>;

/// The global model selection (`augmentagent model`), as Discord reads it
/// for a conversation with no override of its own.
pub fn default_selection() -> ProviderSelection {
    Arc::new(|_| {
        let store = augmentagent_channel_core::model_selection::SelectionStore::new(
            augmentagent_channel_core::model_selection::config_path(),
        );
        store.selected(None)
    })
}

pub struct SlackConversationHarness {
    store: Arc<Store>,
    agent: Arc<dyn QueryHandler>,
    wiki_root: PathBuf,
    selection: ProviderSelection,
}

impl SlackConversationHarness {
    pub fn new(store: Arc<Store>, agent: Arc<dyn QueryHandler>, wiki_root: PathBuf) -> Self {
        Self {
            store,
            agent,
            wiki_root,
            selection: default_selection(),
        }
    }

    pub fn with_selection(mut self, selection: ProviderSelection) -> Self {
        self.selection = selection;
        self
    }

    /// A turn that was already claimed: never submit it again.
    fn replayed(&self, turn: &SurfaceTurnRef) -> anyhow::Result<Option<&'static str>> {
        let Some(state) = self.store.surface_turn_state(turn)? else {
            return Ok(None);
        };
        Ok(Some(match (state.status, state.resolution) {
            (_, Some(SurfaceTurnResolution::Cancelled)) => CANCELLED_REPLY,
            (_, Some(SurfaceTurnResolution::Interrupted)) => INTERRUPTED_REPLY,
            (SurfaceTurnStatus::Complete, None) => FINISHED_BEFORE_RESTART_REPLY,
            (SurfaceTurnStatus::Pending | SurfaceTurnStatus::Uncertain, None) => {
                self.store
                    .resolve_surface_turn(turn, SurfaceTurnResolution::Interrupted)?;
                info!(
                    turn = turn.turn_id(),
                    "slack harness: interrupted turn resolved, not re-run"
                );
                INTERRUPTED_REPLY
            }
        }))
    }

    /// Answer text and validated `ATTACH:` files (Discord's rules: inside the
    /// wiki root only; every refusal is a visible note).
    fn deliverable(&self, answer: &str) -> SlackTurnReply {
        let extracted = extract_attach_markers(answer, Some(&self.wiki_root));
        let mut text = extracted.text;
        let mut notes = extracted.notes;
        notes.extend(augmentagent_approval_discord::register::audit_register_receipts(&text));
        for note in notes {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&format!("⚠️ {note}"));
        }
        let files = extracted
            .files
            .into_iter()
            .map(|path| AnswerFile {
                path,
                filename: None,
                title: None,
                alt_text: None,
            })
            .collect::<Vec<_>>();
        if text.trim().is_empty() && !files.is_empty() {
            text = "📎".into();
        }
        SlackTurnReply { text, files }
    }
}

fn reply(text: &str) -> Option<SlackTurnReply> {
    Some(SlackTurnReply {
        text: text.to_string(),
        files: Vec::new(),
    })
}

#[async_trait]
impl SlackTurnHandler for SlackConversationHarness {
    async fn handle_turn(&self, turn: &SlackTurn) -> anyhow::Result<Option<SlackTurnReply>> {
        // Buttons and modals are #1289.
        if turn.source == OwnerInputSource::Interaction || turn.prompt.trim().is_empty() {
            return Ok(None);
        }
        let Some(session) = &turn.session else {
            return Ok(None);
        };
        let claim = SurfaceTurnRef::new(session.clone(), turn.turn_id.clone())?;
        if let Some(text) = self.replayed(&claim)? {
            return Ok(reply(text));
        }
        let ctx = AuditCtx {
            session_id: turn.turn_id.clone(),
            guild_id: None,
            http: None,
            channel_id: None,
            owner_authorized: true,
        };
        let env: Vec<(String, String)> = turn
            .inbound_dir
            .iter()
            .filter_map(|dir| dir.to_str())
            .map(|dir| (SLACK_INBOUND_DIR_ENV.to_string(), dir.to_string()))
            .collect();
        let cwd = self.wiki_root.to_string_lossy().into_owned();
        let selection = Arc::clone(&self.selection);
        let outcome = TURN_ENV
            .scope(
                env,
                run_surface_turn(
                    &self.store,
                    SurfaceTurnRequest {
                        turn: &claim,
                        history: "",
                        current: turn.prompt.trim(),
                        cwd: &cwd,
                    },
                    || selection(session),
                    Some(&turn.cancel),
                    |prompt| {
                        let ctx = &ctx;
                        async move { self.agent.answer(ctx, &prompt).await }
                    },
                ),
            )
            .await;
        match outcome {
            Ok(SurfaceTurnOutcome::Answered(answer)) => Ok(Some(self.deliverable(&answer))),
            Ok(SurfaceTurnOutcome::Cancelled) => {
                info!(turn = %turn.turn_id, "slack harness: turn cancelled by the owner");
                Ok(reply(CANCELLED_REPLY))
            }
            Err(e) if e.to_string().contains("uncertain") => {
                warn!(turn = %turn.turn_id, error = %e, "slack harness: conversation session is uncertain");
                Ok(reply(SESSION_UNCERTAIN_REPLY))
            }
            Err(e) => Err(e),
        }
    }
}
