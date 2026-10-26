//! Authorized per-call coordination. Audio and native task execution remain
//! independent: this layer only persists intent and wakes the existing worker.
use crate::{
    live_context::LiveContext,
    live_session::{LiveError, LiveInput},
    PhoneService,
};
use augmentagent_store::phone::PhoneLiveResult;
use std::{collections::HashMap, sync::Arc};

pub enum LiveAction {
    None,
    Queued { task_id: String },
    Clarify { delegation_id: String },
}
pub struct LiveCall {
    service: Arc<PhoneService>,
    call_id: String,
    session_id: String,
    context: LiveContext,
    replies: HashMap<String, String>,
}
impl LiveCall {
    pub fn new(
        service: Arc<PhoneService>,
        call_id: &str,
        session_id: &str,
    ) -> Result<Self, LiveError> {
        service
            .authorize_call(call_id)
            .map_err(|_| LiveError::Access)?;
        service
            .store
            .phone_bind_live_session(call_id, session_id)
            .map_err(|_| LiveError::Protocol)?;
        Ok(Self {
            service,
            call_id: call_id.into(),
            session_id: session_id.into(),
            context: LiveContext::default(),
            replies: HashMap::new(),
        })
    }
    pub fn event(&mut self, input: &LiveInput) -> Result<LiveAction, LiveError> {
        self.service
            .authorize_call(&self.call_id)
            .map_err(|_| LiveError::Access)?;
        match input {
            LiveInput::Transcript {
                event_id,
                user,
                text,
                start_ms,
                end_ms,
            } => {
                self.context.push(input)?;
                self.service
                    .store
                    .phone_append_live_transcript(
                        &self.call_id,
                        &self.session_id,
                        &augmentagent_store::phone::LiveTranscript {
                            event_id: event_id.clone(),
                            user: *user,
                            text: text.clone(),
                            start_ms: *start_ms,
                            end_ms: *end_ms,
                        },
                    )
                    .map_err(|_| LiveError::Unavailable)?;
            }
            LiveInput::Delegation { id, offset_ms } => {
                let Some(description) = self.context.request(*offset_ms)? else {
                    return Ok(LiveAction::Clarify {
                        delegation_id: id.clone(),
                    });
                };
                let task_id = self
                    .service
                    .store
                    .phone_enqueue_live_dispatch(&self.call_id, &self.session_id, id, &description)
                    .map_err(|_| LiveError::Unavailable)?;
                self.service.dispatch_ready.notify_one();
                return Ok(LiveAction::Queued { task_id });
            }
            LiveInput::CommentaryAccepted { client_event_id } => {
                if let Some(delegation) = self.replies.get(client_event_id) {
                    self.service
                        .store
                        .phone_live_result_delivered(&self.call_id, &self.session_id, delegation)
                        .map_err(|_| LiveError::Unavailable)?;
                    self.replies.remove(client_event_id);
                }
            }
            _ => {}
        }
        Ok(LiveAction::None)
    }
    pub fn pending_results(&self) -> Result<Vec<PhoneLiveResult>, LiveError> {
        self.service
            .authorize_call(&self.call_id)
            .map_err(|_| LiveError::Access)?;
        self.service
            .store
            .phone_live_results(&self.call_id, &self.session_id)
            .map_err(|_| LiveError::Unavailable)
    }
    /// Register only a successfully sent commentary command. Its acceptance is
    /// separate from audio playback and never advances playback metrics.
    pub fn track_reply(&mut self, event_id: &str, delegation_id: &str) -> Result<(), LiveError> {
        self.service
            .authorize_call(&self.call_id)
            .map_err(|_| LiveError::Access)?;
        if event_id.is_empty() || event_id.len() > 256 || self.replies.len() >= 128 {
            return Err(LiveError::Capacity);
        }
        if !self
            .pending_results()?
            .iter()
            .any(|result| result.delegation_id == delegation_id)
        {
            return Err(LiveError::Protocol);
        }
        if self.replies.contains_key(event_id)
            || self.replies.values().any(|id| id == delegation_id)
        {
            return Err(LiveError::Protocol);
        }
        self.replies.insert(event_id.into(), delegation_id.into());
        Ok(())
    }
    pub fn reply_pending(&self, delegation_id: &str) -> bool {
        self.replies.values().any(|id| id == delegation_id)
    }
}
impl Drop for LiveCall {
    fn drop(&mut self) {
        // Native jobs are intentionally untouched; their original chat delivery
        // and authorization checks remain the existing worker's responsibility.
        if self
            .service
            .store
            .phone_end_live_session(&self.call_id, &self.session_id)
            .is_err()
        {
            tracing::warn!("Live session delivery cleanup deferred");
        }
    }
}
