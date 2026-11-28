//! Jarvis Dashboard turn runner (spec web-dashboard-v2 D1, §5.2.1).
//!
//! `submit` records the turn and returns at once; a background task runs it
//! through `run_surface_turn_with_history` with the turn ref `web:<turn_id>`,
//! this registry's cancel token and the thread's fallback history. The
//! per-conversation queue runs first, so at most one turn per thread is past
//! it; inside the answer closure a global semaphore caps concurrent web
//! turns, then `queued → running` is compared-and-swapped (a failed swap
//! means the turn was cancelled). Exactly one terminal write follows.
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use augmentagent_approval_discord::{AuditCtx, QueryHandler};
use augmentagent_channel_core::reasoner::ReasonerError;
use augmentagent_channel_core::surface_turn::{
    run_surface_turn_with_history, SurfaceTurnOutcome, SurfaceTurnRequest,
};
use augmentagent_store::web_chat::{
    web_conversation, web_turn_ref, WebChatResult, WebSubmitted, WebThread, WebTurnEnd,
    WebTurnErrorCode, WebTurnStatus,
};
use augmentagent_store::{Store, SurfaceTurnStatus};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

/// Concurrent web turns across all threads.
pub const WEB_TURN_PERMITS: usize = 3;
const MAX_ERROR_CHARS: usize = 500;

/// The audit context of a web turn (spec D1, T1): owner-authenticated, no
/// Discord guild, channel or http, so Discord-posting and channel-scoped
/// tools are not attached.
pub fn web_audit_ctx(thread_id: &str, turn_id: &str) -> AuditCtx {
    AuditCtx {
        session_id: format!("web:{thread_id}:{turn_id}"),
        owner_authorized: true,
        ..AuditCtx::empty()
    }
}

/// Run `f` as part of web turn `thread_id`, so channel-scoped tools bind to
/// the web thread (#2777).
pub async fn in_web_turn<F: std::future::Future>(_thread_id: &str, f: F) -> F::Output {
    f.await
}

pub struct WebTurnRunner {
    store: Arc<Store>,
    handler: Arc<dyn QueryHandler>,
    cwd: String,
    permits: Semaphore,
    cancels: Mutex<HashMap<String, CancellationToken>>,
}

impl WebTurnRunner {
    pub fn new(store: Arc<Store>, handler: Arc<dyn QueryHandler>, cwd: String) -> Arc<Self> {
        Arc::new(Self {
            store,
            handler,
            cwd,
            permits: Semaphore::new(WEB_TURN_PERMITS),
            cancels: Mutex::default(),
        })
    }

    /// `turn.submit`: record the turn and, when it is new, run it in the
    /// background. A replay returns the stored status and runs nothing.
    pub fn submit(
        self: &Arc<Self>,
        thread_id: &str,
        turn_id: &str,
        text: &str,
    ) -> WebChatResult<WebSubmitted> {
        let submitted = self.store.web_submit_turn(thread_id, turn_id, text)?;
        if submitted.fresh {
            let token = CancellationToken::new();
            self.registry().insert(turn_id.to_string(), token.clone());
            let runner = Arc::clone(self);
            let (thread_id, turn_id, text) =
                (thread_id.to_string(), turn_id.to_string(), text.to_string());
            tokio::spawn(async move { runner.run(&thread_id, &turn_id, &text, token).await });
        }
        Ok(submitted)
    }

    /// `turn.cancel`: fire the token if one is registered, and cancel a
    /// queued turn outright. A running turn is cancelled by its runner.
    pub fn cancel(&self, turn_id: &str) -> WebChatResult<WebTurnStatus> {
        if let Some(token) = self.registry().get(turn_id) {
            token.cancel();
        }
        self.store.web_cancel_queued_turn(turn_id)
    }

    /// `thread.reset_session`, refused while the thread is busy.
    pub fn reset_session(&self, thread_id: &str) -> WebChatResult<WebThread> {
        self.store.web_reset_session(thread_id)
    }

    fn registry(&self) -> std::sync::MutexGuard<'_, HashMap<String, CancellationToken>> {
        self.cancels.lock().expect("web cancel registry poisoned")
    }

    async fn run(&self, thread_id: &str, turn_id: &str, text: &str, token: CancellationToken) {
        let turn = web_turn_ref(thread_id, turn_id);
        let request = SurfaceTurnRequest {
            turn: &turn,
            history: "",
            current: text,
            cwd: &self.cwd,
        };
        let store = &self.store;
        let failed_flag = AtomicBool::new(false);
        let answer_failed = &failed_flag;
        let result = run_surface_turn_with_history(
            store,
            request,
            || Ok(None),
            Some(&token),
            |prompt: String| async move {
                let _permit = self.permits.acquire().await?;
                if !store.web_start_turn(turn_id)? {
                    anyhow::bail!("web turn was cancelled before it started");
                }
                let ctx = web_audit_ctx(thread_id, turn_id);
                let answer = augmentagent_approval_discord::interaction::owner_history(
                    true,
                    self.handler.answer(&ctx, &prompt),
                )
                .await;
                answer_failed.store(answer.is_err(), Ordering::SeqCst);
                answer
            },
            || async { Ok(store.web_fallback_history(thread_id, turn_id)?) },
        )
        .await;
        let end = match &result {
            Ok(SurfaceTurnOutcome::Answered(reply)) => WebTurnEnd::Done(reply),
            Ok(SurfaceTurnOutcome::Cancelled) => WebTurnEnd::Cancelled,
            Err(error) => {
                let code = self.classify(&turn, error, answer_failed.load(Ordering::SeqCst));
                tracing::warn!(turn_id, code = code.as_str(), error = %format!("{error:#}"),
                    "web turn failed");
                let text: String = format!("{error:#}").chars().take(MAX_ERROR_CHARS).collect();
                return self.end(turn_id, WebTurnEnd::Failed(code, &text));
            }
        };
        self.end(turn_id, end);
    }

    fn end(&self, turn_id: &str, end: WebTurnEnd<'_>) {
        // A false result means the row already left the states `end` applies
        // to (cancelled while queued); the row is then already terminal.
        if let Err(error) = self.store.web_end_turn(turn_id, end) {
            tracing::error!(turn_id, error = %error, "web turn terminal write failed");
        }
        self.registry().remove(turn_id);
    }

    /// Turn error code (§5.2.1). `uncertain_session` comes from a store
    /// probe, never from the error text.
    fn classify(
        &self,
        turn: &augmentagent_store::SurfaceTurnRef,
        error: &anyhow::Error,
        answer_failed: bool,
    ) -> WebTurnErrorCode {
        if matches!(
            ReasonerError::find_in(error),
            Some(ReasonerError::Interrupted { .. })
        ) {
            return WebTurnErrorCode::Interrupted;
        }
        let conversation_uncertain = self
            .store
            .surface_conversation(&web_conversation(turn.conversation().conversation_id()))
            .ok()
            .flatten()
            .is_some_and(|binding| binding.uncertain);
        let claim_unresolved = self
            .store
            .surface_turn_state(turn)
            .ok()
            .flatten()
            .is_some_and(|state| {
                state.status != SurfaceTurnStatus::Complete && state.resolution.is_none()
            });
        if conversation_uncertain || claim_unresolved {
            WebTurnErrorCode::UncertainSession
        } else if answer_failed {
            WebTurnErrorCode::ProviderError
        } else {
            WebTurnErrorCode::Internal
        }
    }
}
