//! Per-turn latency instrumentation (#1551). A `TurnTimer` starts when the final
//! `prompt` frame is received and collects monotonic marks from the ingress
//! loop and the agent worker. It never sees speech text; the finished record is
//! the only thing persisted or logged.
use augmentagent_store::phone::{PhoneTurnMetrics, TurnOutcome, TurnProvider};
use std::{
    sync::Mutex,
    time::Duration,
};
use tokio::time::Instant;

#[derive(Default)]
struct Marks {
    dequeued: Option<Instant>,
    first_token: Option<Instant>,
    first_speech: Option<Instant>,
    last_speech: Option<Instant>,
    first_played: Option<Instant>,
    last_played: Option<Instant>,
    tool: Duration,
    /// Time spent waiting for the shared conversation queue (#1548).
    queue: Duration,
    fast: bool,
    dispatched: bool,
    speculated: bool,
    speculation_saved: Option<Duration>,
}
pub struct TurnTimer {
    started: Instant,
    marks: Mutex<Marks>,
}
fn offset(started: Instant, at: Option<Instant>) -> Option<i64> {
    at.map(|at| i64::try_from(at.saturating_duration_since(started).as_millis()).unwrap_or(i64::MAX))
}
impl TurnTimer {
    /// Call on receipt of the final prompt frame.
    pub fn start() -> Self {
        Self {
            started: Instant::now(),
            marks: Mutex::new(Marks::default()),
        }
    }
    fn update(&self, f: impl FnOnce(&mut Marks)) {
        let mut marks = self.marks.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        f(&mut marks);
    }
    /// The worker picked the turn up from the queue.
    pub fn dequeued(&self) {
        self.update(|m| {
            m.dequeued.get_or_insert_with(Instant::now);
        });
    }
    /// The agent produced its first output (token or whole answer). Idempotent.
    pub fn first_token(&self) {
        self.update(|m| {
            m.first_token.get_or_insert_with(Instant::now);
        });
    }
    /// A user-facing speech frame was written to the socket.
    pub fn speech_written(&self, final_output: bool) {
        self.update(|m| {
            let now = Instant::now();
            m.first_speech.get_or_insert(now);
            if final_output {
                m.last_speech = Some(now);
            }
        });
    }
    /// Provider reported tokens actually played (Relay debug events).
    pub fn played(&self, at: Instant) {
        self.update(|m| {
            m.first_played.get_or_insert(at);
            m.last_played = Some(m.last_played.map_or(at, |old| old.max(at)));
        });
    }
    /// The fast layer (#1546) answered this turn.
    pub fn mark_fast(&self) {
        self.update(|m| m.fast = true);
    }
    /// This turn handed work off with `dispatch_task` (#1546).
    pub fn mark_dispatched(&self) {
        self.update(|m| m.dispatched = true);
    }
    /// Preemptive generation (#1555): whether this utterance started a
    /// speculation, and the time saved when it was committed.
    pub fn speculation(&self, speculated: bool, saved: Option<Duration>) {
        self.update(|m| {
            m.speculated = speculated;
            m.speculation_saved = saved;
        });
    }
    /// The committed speculation could not stand as the answer (it wanted a
    /// tool or failed before speaking): the turn re-planned from scratch.
    pub fn speculation_discarded(&self) {
        self.update(|m| m.speculation_saved = None);
    }
    /// Provider for the record: fast when the fast layer answered.
    pub fn provider(&self) -> TurnProvider {
        if self.marks.lock().unwrap_or_else(|p| p.into_inner()).fast {
            TurnProvider::Fast
        } else {
            TurnProvider::Native
        }
    }
    /// An answered turn that dispatched work is recorded as `dispatched`.
    pub fn outcome(&self, outcome: TurnOutcome) -> TurnOutcome {
        let dispatched = self.marks.lock().unwrap_or_else(|p| p.into_inner()).dispatched;
        match outcome {
            TurnOutcome::Answered if dispatched => TurnOutcome::Dispatched,
            other => other,
        }
    }
    /// Time the turn waited for the shared conversation queue (#1548); added
    /// to the worker pickup wait in `queue_wait_ms`.
    pub fn add_queue_wait(&self, duration: Duration) {
        self.update(|m| m.queue = m.queue.saturating_add(duration));
    }
    pub fn add_tool_time(&self, duration: Duration) {
        self.update(|m| m.tool = m.tool.saturating_add(duration));
    }
    pub fn has_speech(&self) -> bool {
        self.marks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .first_speech
            .is_some()
    }
    pub fn record(
        &self,
        call_id: &str,
        turn_id: &str,
        provider: TurnProvider,
        outcome: TurnOutcome,
        recorded_ms: i64,
        commit_sha: Option<&str>,
    ) -> PhoneTurnMetrics {
        let marks = self.marks.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let t_first_speech_ms = offset(self.started, marks.first_speech);
        let t_first_played_ms = offset(self.started, marks.first_played);
        PhoneTurnMetrics {
            call_id: call_id.into(),
            turn_id: turn_id.into(),
            recorded_ms,
            provider,
            outcome,
            // Worker pickup plus any wait for the shared conversation queue.
            queue_wait_ms: offset(self.started, marks.dequeued).map(|pickup| {
                pickup.saturating_add(i64::try_from(marks.queue.as_millis()).unwrap_or(i64::MAX))
            }),
            t_first_token_ms: offset(self.started, marks.first_token),
            t_first_speech_ms,
            t_last_speech_ms: offset(self.started, marks.last_speech),
            tool_ms: (marks.tool > Duration::ZERO)
                .then(|| i64::try_from(marks.tool.as_millis()).unwrap_or(i64::MAX)),
            t_first_played_ms,
            t_last_played_ms: offset(self.started, marks.last_played),
            audible_delta_ms: t_first_played_ms.zip(t_first_speech_ms).map(|(p, s)| p - s),
            commit_sha: commit_sha.map(str::to_owned),
            speculated: marks.speculated,
            speculation_committed: marks.speculation_saved.is_some(),
            speculation_saved_ms: marks
                .speculation_saved
                .map(|saved| i64::try_from(saved.as_millis()).unwrap_or(i64::MAX)),
        }
    }
}

/// Git commit of the build, captured by `build.rs`; `None` outside a checkout.
pub fn build_commit() -> Option<&'static str> {
    option_env!("AUGMENTAGENT_PHONE_BUILD_COMMIT").filter(|sha| {
        (7..=40).contains(&sha.len()) && sha.bytes().all(|b| b.is_ascii_hexdigit())
    })
}
