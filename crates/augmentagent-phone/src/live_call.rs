//! Authorized per-call coordination. Audio and native task execution remain
//! independent: this layer only persists intent and wakes the existing worker.
use crate::{
    live_context::LiveContext,
    live_session::{LiveError, LiveInput},
    PhoneService,
};
use augmentagent_store::phone::PhoneLiveResult;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
};

pub enum LiveAction {
    None,
    Queued {
        task_id: String,
        delegation_id: String,
    },
    Clarify {
        delegation_id: String,
    },
    QueueFull {
        delegation_id: String,
    },
}
struct Reply {
    delegation: String,
    end: usize,
    deadline: tokio::time::Instant,
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
    last_result: Option<String>,
    context_results: HashSet<String>,
    expired_replies: Vec<String>,
    notices: Vec<(String, String, tokio::time::Instant, bool)>,
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
            last_result: None,
            context_results: HashSet::new(),
            expired_replies: Vec::new(),
            notices: Vec::new(),
            pending_delegations: VecDeque::new(),
            last_user_fragment: None,
        })
    }
    pub fn event(&mut self, input: &LiveInput) -> Result<LiveAction, LiveError> {
        self.service
            .authorize_call(&self.call_id)
            .map_err(|_| LiveError::Access)?;
        self.expire_replies()?;
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
                    let accepted = self
                        .service
                        .store
                        .phone_live_reply_settle(
                            &self.call_id,
                            &self.session_id,
                            client_event_id,
                            true,
                        )
                        .map_err(|_| LiveError::Unavailable)?;
                    if accepted {
                        self.result_offsets
                            .insert(reply.delegation.clone(), reply.end);
                        self.replies.remove(client_event_id);
                    }
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
        self.expire_replies()?;
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
        self.notices.push((
            id.clone(),
            task_id.clone(),
            tokio::time::Instant::now(),
            false,
        ));
        tracing::info!(call_id=%self.call_id,task_id=%task_id,offset_hint_ms=offset,"phone live request accepted for dispatch");
        Ok(LiveAction::Queued {
            task_id,
            delegation_id: id,
        })
    }
    pub fn delay_notice(&mut self) -> Result<Option<PhoneLiveResult>, LiveError> {
        self.service
            .authorize_call(&self.call_id)
            .map_err(|_| LiveError::Access)?;
        if !self.replies.is_empty() || !self.pending_results()?.is_empty() {
            return Ok(None);
        }
        let jobs = self
            .service
            .store
            .phone_pending_dispatches()
            .map_err(|_| LiveError::Unavailable)?;
        for (delegation, task, started, notified) in &mut self.notices {
            if *notified || started.elapsed() < std::time::Duration::from_secs(10) {
                continue;
            }
            *notified = true;
            let Some(job) = jobs
                .iter()
                .find(|j| j.call_id == self.call_id && j.task_id == *task)
            else {
                continue;
            };
            let phase = match job.state.as_str() {
                "queued" => "still queued",
                "running" => "still running",
                _ => continue,
            };
            return Ok(Some(PhoneLiveResult{delegation_id:delegation.clone(),result:format!("The task is {phase}; I don't have its verified findings yet. We can keep talking while it finishes.")}));
        }
        Ok(None)
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
        if let Some(index) = self
            .last_result
            .as_ref()
            .and_then(|last| results.iter().position(|r| &r.delegation_id == last))
        {
            let next = (index + 1) % results.len();
            results.rotate_left(next);
        }
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
    /// Persist send intent BEFORE socket I/O. Acceptance is distinct from
    /// playback; a crash after this point cannot automatically resend a piece.
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
        self.service
            .store
            .phone_live_reply_intent(
                &self.call_id,
                &self.session_id,
                delegation_id,
                event_id,
                end,
                end == result.result.len(),
            )
            .map_err(|_| LiveError::Unavailable)?;
        self.last_result = Some(delegation_id.into());
        self.replies.insert(
            event_id.into(),
            Reply {
                delegation: delegation_id.into(),
                end,
                deadline: tokio::time::Instant::now() + std::time::Duration::from_secs(5),
            },
        );
        Ok(())
    }
    fn expire_replies(&mut self) -> Result<(), LiveError> {
        let expired: Vec<_> = self
            .replies
            .iter()
            .filter(|(_, r)| tokio::time::Instant::now() >= r.deadline)
            .map(|(id, r)| (id.clone(), r.delegation.clone()))
            .collect();
        for (id, delegation) in expired {
            self.service
                .store
                .phone_live_result_timeout(&self.call_id, &self.session_id, &delegation)
                .map_err(|_| LiveError::Unavailable)?;
            self.replies.remove(&id);
            self.expired_replies.push(id.clone());
            tracing::warn!(call_id=%self.call_id, event_id=%id, "phone result context acknowledgment timed out; publication abandoned, chat retained");
        }
        Ok(())
    }
    /// Only a provider-proven nonaccepted delegation reference may be repaired.
    /// Remaining pieces then use general session context, not the lost ID.
    pub fn rejected_delegation(
        &mut self,
        event_id: &str,
    ) -> Result<Option<PhoneLiveResult>, LiveError> {
        self.expire_replies()?;
        let Some(reply) = self.replies.get(event_id) else {
            return Ok(None);
        };
        let delegation = reply.delegation.clone();
        if self.context_results.contains(&delegation) {
            return Err(LiveError::Protocol);
        }
        if !self
            .service
            .store
            .phone_live_reply_settle(&self.call_id, &self.session_id, event_id, false)
            .map_err(|_| LiveError::Unavailable)?
        {
            return Ok(None);
        }
        self.replies.remove(event_id);
        self.context_results.insert(delegation.clone());
        Ok(self
            .pending_results()?
            .into_iter()
            .find(|r| r.delegation_id == delegation))
    }
    pub fn uses_session_context(&self, delegation_id: &str) -> bool {
        self.context_results.contains(delegation_id)
    }
    pub fn take_expired_replies(&mut self) -> Vec<String> {
        std::mem::take(&mut self.expired_replies)
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
