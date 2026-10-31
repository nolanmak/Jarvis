//! Authorized per-call coordination. Audio and native task execution remain
//! independent: this layer only persists intent and wakes the existing worker.
use crate::{
    live_context::LiveContext,
    live_session::{LiveError, LiveInput},
    PhoneService,
};
use augmentagent_store::phone::PhoneLiveResult;
use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};

pub enum LiveAction {
    None,
    Queued { task_id: String },
    Clarify { delegation_id: String },
    QueueFull { delegation_id: String },
}
struct Reply {
    delegation: String,
    end: usize,
    complete: bool,
}

/// Coherent bounded pieces, with exact UTF-8 preservation. The native agent's
/// original result stays in the chat outbox; Live handles spoken paraphrasing.
fn result_part(text: &str, offset: usize) -> (&str, usize) {
    let mut end = (offset + 500).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    if end < text.len() {
        let piece = &text[offset..end];
        let boundary = piece
            .char_indices()
            .filter(|(i, c)| *i >= 100 && matches!(c, '\n' | '.' | '!' | '?'))
            .map(|(i, c)| i + c.len_utf8())
            .next_back()
            .or_else(|| {
                piece
                    .char_indices()
                    .filter(|(i, c)| *i >= 100 && c.is_whitespace())
                    .map(|(i, c)| i + c.len_utf8())
                    .next_back()
            });
        if let Some(boundary) = boundary {
            end = offset + boundary;
        }
    }
    (&text[offset..end], end)
}

pub struct LiveCall {
    service: Arc<PhoneService>,
    call_id: String,
    session_id: String,
    context: LiveContext,
    replies: HashMap<String, Reply>,
    result_offsets: HashMap<String, usize>,
    pending_delegations: VecDeque<(String, f64, tokio::time::Instant, bool)>,
    last_user_fragment: Option<tokio::time::Instant>,
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
            result_offsets: HashMap::new(),
            pending_delegations: VecDeque::new(),
            last_user_fragment: None,
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
                if *user {
                    self.last_user_fragment = Some(tokio::time::Instant::now());
                }
                let stored = self.service.store.phone_append_live_transcript(
                    &self.call_id,
                    &self.session_id,
                    &augmentagent_store::phone::LiveTranscript {
                        event_id: event_id.clone(),
                        user: *user,
                        text: text.clone(),
                        start_ms: *start_ms,
                        end_ms: *end_ms,
                    },
                );
                match stored {
                    Ok(_) => {}
                    // The store commits an incomplete-history marker atomically
                    // with this refusal. Audio and native tasks remain live.
                    Err(augmentagent_store::phone::PhoneError::RateLimited) => {}
                    Err(_) => return Err(LiveError::Unavailable),
                }
            }
            LiveInput::Delegation { id, offset_ms } => {
                if self.pending_delegations.len() >= 128 {
                    return Err(LiveError::Capacity);
                }
                let waiting = self.context.clone().request(*offset_ms)?.is_none();
                self.pending_delegations.push_back((
                    id.clone(),
                    *offset_ms,
                    tokio::time::Instant::now() + std::time::Duration::from_secs(2),
                    waiting,
                ));
                return self.poll_delegations();
            }
            LiveInput::CommentaryAccepted { client_event_id } => {
                if let Some(reply) = self.replies.get(client_event_id) {
                    if reply.complete {
                        self.service
                            .store
                            .phone_live_result_delivered(
                                &self.call_id,
                                &self.session_id,
                                &reply.delegation,
                            )
                            .map_err(|_| LiveError::Unavailable)?;
                    }
                    self.result_offsets
                        .insert(reply.delegation.clone(), reply.end);
                    self.replies.remove(client_event_id);
                }
            }
            _ => {}
        }
        self.poll_delegations()
    }
    /// Transcripts can arrive after delegation metadata. Wait briefly without
    /// blocking audio; commit the consumed watermark only on dispatch/refusal.
    pub fn poll_delegations(&mut self) -> Result<LiveAction, LiveError> {
        self.service
            .authorize_call(&self.call_id)
            .map_err(|_| LiveError::Access)?;
        let Some((id, offset, deadline, waiting)) = self.pending_delegations.front().cloned()
        else {
            return Ok(LiveAction::None);
        };
        let mut context = self.context.clone();
        let description = context.request(offset)?;
        let now = tokio::time::Instant::now();
        if description.is_none() && now < deadline {
            return Ok(LiveAction::None);
        }
        if description.is_some()
            && waiting
            && self
                .last_user_fragment
                .is_some_and(|at| now.duration_since(at) < std::time::Duration::from_millis(250))
        {
            return Ok(LiveAction::None);
        }
        self.pending_delegations.pop_front();
        self.context = context;
        let Some(description) = description else {
            tracing::warn!(
                "phone delegation has no complete retained request after transcript wait"
            );
            return Ok(LiveAction::Clarify { delegation_id: id });
        };
        let task_id = match self.service.store.phone_enqueue_live_dispatch(
            &self.call_id,
            &self.session_id,
            &id,
            &description,
        ) {
            Ok(task_id) => task_id,
            Err(augmentagent_store::phone::PhoneError::RateLimited) => {
                return Ok(LiveAction::QueueFull { delegation_id: id })
            }
            Err(_) => return Err(LiveError::Unavailable),
        };
        self.service.dispatch_ready.notify_one();
        Ok(LiveAction::Queued { task_id })
    }
    pub fn pending_results(&self) -> Result<Vec<PhoneLiveResult>, LiveError> {
        self.service
            .authorize_call(&self.call_id)
            .map_err(|_| LiveError::Access)?;
        let mut results = self
            .service
            .store
            .phone_live_results(&self.call_id, &self.session_id)
            .map_err(|_| LiveError::Unavailable)?;
        for result in &mut results {
            let offset = self
                .result_offsets
                .get(&result.delegation_id)
                .copied()
                .unwrap_or(0);
            if offset >= result.result.len() {
                return Err(LiveError::Protocol);
            }
            result.result = result_part(&result.result, offset).0.to_owned();
        }
        Ok(results)
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
        let result = self
            .service
            .store
            .phone_live_results(&self.call_id, &self.session_id)
            .map_err(|_| LiveError::Unavailable)?
            .into_iter()
            .find(|r| r.delegation_id == delegation_id)
            .ok_or(LiveError::Protocol)?;
        if self.replies.contains_key(event_id) || self.reply_pending(delegation_id) {
            return Err(LiveError::Protocol);
        }
        let offset = self.result_offsets.get(delegation_id).copied().unwrap_or(0);
        if offset >= result.result.len() {
            return Err(LiveError::Protocol);
        }
        let (_, end) = result_part(&result.result, offset);
        self.replies.insert(
            event_id.into(),
            Reply {
                delegation: delegation_id.into(),
                end,
                complete: end == result.result.len(),
            },
        );
        Ok(())
    }
    pub fn reply_pending(&self, delegation_id: &str) -> bool {
        self.replies
            .values()
            .any(|reply| reply.delegation == delegation_id)
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
