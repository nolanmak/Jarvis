//! Silence and idle policy (#1556). A connected call with nobody talking
//! checks in twice, then says goodbye and ends itself instead of running to
//! the provider time limit.
//!
//! The silence clock starts at the latest of: owner speech (any `prompt`,
//! partial or final, or an `interrupt`), a closing `text` frame written by
//! Jarvis, and a tokens-played event for that speech. Check-ins and the
//! goodbye never restart it. While an owner turn is being answered the clock
//! is held. A spoken progress line for a dispatched task holds it for
//! [`PROGRESS_HOLD`] (the owner is waiting on Jarvis, not absent); the task's
//! spoken result releases the hold and restarts the clock.
use crate::{
    ingress::{ConversationAgent, TaskSpeech},
    Error, PhoneService,
};
use augmentagent_store::phone::PhoneCall;
use std::{collections::HashMap, time::Duration};
use tokio::time::Instant;

/// Seconds of silence before the first check-in. Default 12.
pub const CHECK_IN_1_SECS: &str = "AUGMENTAGENT_PHONE_SILENCE_CHECKIN_1_SECS";
/// Seconds of silence before the second check-in. Default 25.
pub const CHECK_IN_2_SECS: &str = "AUGMENTAGENT_PHONE_SILENCE_CHECKIN_2_SECS";
/// Seconds of silence before the goodbye and `end`. Default 45.
pub const END_SECS: &str = "AUGMENTAGENT_PHONE_SILENCE_END_SECS";
/// Inclusive bounds for every threshold.
pub const MIN_SECS: u64 = 5;
pub const MAX_SECS: u64 = 300;
pub const CHECK_IN_1_LINE: &str = "Still there?";
pub const CHECK_IN_2_LINE: &str = "I'll hang up in a bit if you're done.";
pub const GOODBYE_LINE: &str = "I'll hang up now. Talk to you later.";
/// How long one spoken progress line keeps silence handling suspended.
pub const PROGRESS_HOLD: Duration = Duration::from_secs(15);
/// Durable end reason and `handoffData.reason` for a silence hangup.
pub const END_REASON: &str = "silence";
/// Dispatched tasks tracked for the hold; older entries are dropped first.
const MAX_TASKS: usize = 16;
/// Upper bound for one chat-notice delivery attempt.
const NOTICE_TIMEOUT: Duration = Duration::from_secs(10);

/// Idempotency key of the one chat notice for a runtime-ended call:
/// `{call_id}:silence_end` for a silence hangup.
pub fn end_notice_key(call_id: &str, reason: &str) -> String {
    format!("{call_id}:{reason}_end")
}

impl PhoneService {
    /// Deliver every due end notice once. Each attempt is leased in the store
    /// before the agent runs, and the agent enqueues under a fixed
    /// idempotency key, so a restart never posts the notice twice. Returns
    /// the number delivered by this pass.
    pub async fn run_end_notices(&self, agent: &dyn ConversationAgent) -> Result<usize, Error> {
        if self.stopping.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(0);
        }
        let mut delivered = 0;
        for (call, reason) in self.store.phone_end_notices_due(self.clock.now_ms())? {
            if self.deliver_end_notice(agent, &call, &reason).await? {
                delivered += 1;
            }
        }
        Ok(delivered)
    }
    /// One leased delivery attempt. `false` when another attempt holds the
    /// lease, the notice is already delivered, or this attempt failed (a
    /// bounded retry remains).
    pub(crate) async fn deliver_end_notice(
        &self,
        agent: &dyn ConversationAgent,
        call: &PhoneCall,
        reason: &str,
    ) -> Result<bool, Error> {
        if !self
            .store
            .phone_claim_end_notice(&call.id, self.clock.now_ms())?
        {
            return Ok(false);
        }
        let text = if reason == END_REASON {
            format!(
                "Phone call ended after {} seconds of silence.",
                self.config.silence.end.as_secs()
            )
        } else {
            "Phone call ended by Jarvis.".to_owned()
        };
        let key = end_notice_key(&call.id, reason);
        match tokio::time::timeout(NOTICE_TIMEOUT, agent.call_ended(call, &key, &text)).await {
            Ok(Ok(())) => Ok(self
                .store
                .phone_complete_end_notice(&call.id, self.clock.now_ms())?),
            _ => {
                tracing::warn!(call_id = %call.id, reason, "phone end notice not delivered; a bounded retry remains");
                Ok(false)
            }
        }
    }
}

/// Typed configuration failure. Carries the variable name, never its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SilenceConfigError {
    #[error("{0} must be a whole number of seconds")]
    Invalid(&'static str),
    #[error("{0} must be between 5 and 300 seconds")]
    OutOfRange(&'static str),
    #[error("silence thresholds must be strictly increasing")]
    NotIncreasing,
}

/// Validated thresholds, measured from the start of the silence clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SilencePolicy {
    check_in_1: Duration,
    check_in_2: Duration,
    end: Duration,
}
impl Default for SilencePolicy {
    fn default() -> Self {
        Self {
            check_in_1: Duration::from_secs(12),
            check_in_2: Duration::from_secs(25),
            end: Duration::from_secs(45),
        }
    }
}
impl SilencePolicy {
    /// Each threshold in 5–300 s, strictly increasing.
    pub fn new(check_in_1: u64, check_in_2: u64, end: u64) -> Result<Self, SilenceConfigError> {
        for (key, secs) in [
            (CHECK_IN_1_SECS, check_in_1),
            (CHECK_IN_2_SECS, check_in_2),
            (END_SECS, end),
        ] {
            if !(MIN_SECS..=MAX_SECS).contains(&secs) {
                return Err(SilenceConfigError::OutOfRange(key));
            }
        }
        if check_in_1 >= check_in_2 || check_in_2 >= end {
            return Err(SilenceConfigError::NotIncreasing);
        }
        Ok(Self {
            check_in_1: Duration::from_secs(check_in_1),
            check_in_2: Duration::from_secs(check_in_2),
            end: Duration::from_secs(end),
        })
    }
    /// Absent variables keep their defaults; junk fails closed.
    pub fn from_lookup(get: &impl Fn(&str) -> Option<String>) -> Result<Self, SilenceConfigError> {
        let defaults = Self::default();
        let secs = |key: &'static str, default: Duration| match get(key) {
            None => Ok(default.as_secs()),
            Some(value) => value
                .trim()
                .parse::<u64>()
                .map_err(|_| SilenceConfigError::Invalid(key)),
        };
        Self::new(
            secs(CHECK_IN_1_SECS, defaults.check_in_1)?,
            secs(CHECK_IN_2_SECS, defaults.check_in_2)?,
            secs(END_SECS, defaults.end)?,
        )
    }
    pub fn check_in_1(&self) -> Duration {
        self.check_in_1
    }
    pub fn check_in_2(&self) -> Duration {
        self.check_in_2
    }
    pub fn end(&self) -> Duration {
        self.end
    }
}

/// What the relay should say next. `Goodbye` is followed by `end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SilenceAction {
    CheckIn(&'static str),
    Goodbye(&'static str),
}

/// Per-connection silence clock. Pure state: the relay feeds it events and
/// polls it on its tick, so every rule is testable on a paused clock.
pub struct SilenceMonitor {
    policy: SilencePolicy,
    anchor: Instant,
    /// Actions already taken since the clock last restarted (0–3).
    stage: u8,
    /// Dispatched tasks with a spoken progress line and no spoken result.
    progress: HashMap<String, Instant>,
}
impl SilenceMonitor {
    pub fn new(policy: SilencePolicy, now: Instant) -> Self {
        Self {
            policy,
            anchor: now,
            stage: 0,
            progress: HashMap::new(),
        }
    }
    /// Owner speech, a closing frame of Jarvis speech, or a held clock
    /// (an owner turn still being answered). Restarts the clock.
    pub fn activity(&mut self, now: Instant) {
        if self.stage < 3 {
            self.anchor = self.anchor.max(now);
            self.stage = 0;
        }
    }
    /// Tokens-played event. Counts only for speech that was not a check-in,
    /// so the provider confirming a check-in never restarts the clock.
    pub fn played(&mut self, now: Instant) {
        if self.stage == 0 {
            self.anchor = self.anchor.max(now);
        }
    }
    /// A dispatched task's progress or result line was written.
    pub fn task_spoken(&mut self, speech: &TaskSpeech, now: Instant) {
        match speech {
            TaskSpeech::Progress(task) => {
                if self.progress.len() >= MAX_TASKS && !self.progress.contains_key(task) {
                    self.release_oldest();
                }
                self.progress.insert(task.clone(), now);
            }
            TaskSpeech::Result(task) => {
                self.progress.remove(task);
            }
        }
        self.activity(now);
    }
    fn release_oldest(&mut self) {
        if let Some(task) = self
            .progress
            .iter()
            .min_by_key(|(_, at)| **at)
            .map(|(task, _)| task.clone())
        {
            if let Some(at) = self.progress.remove(&task) {
                self.anchor = self.anchor.max(at + PROGRESS_HOLD);
            }
        }
    }
    /// The next action due at `now`, at most one per call. Lapsed progress
    /// holds move the clock start to where they lapsed.
    pub fn poll(&mut self, now: Instant) -> Option<SilenceAction> {
        let anchor = &mut self.anchor;
        self.progress.retain(|_, at| {
            let lapses = *at + PROGRESS_HOLD;
            if lapses <= now {
                *anchor = (*anchor).max(lapses);
                false
            } else {
                true
            }
        });
        if !self.progress.is_empty() || now < self.anchor {
            return None;
        }
        let silent = now - self.anchor;
        let (due, action) = match self.stage {
            0 => (
                self.policy.check_in_1,
                SilenceAction::CheckIn(CHECK_IN_1_LINE),
            ),
            1 => (
                self.policy.check_in_2,
                SilenceAction::CheckIn(CHECK_IN_2_LINE),
            ),
            2 => (self.policy.end, SilenceAction::Goodbye(GOODBYE_LINE)),
            _ => return None,
        };
        if silent < due {
            return None;
        }
        self.stage += 1;
        Some(action)
    }
}
