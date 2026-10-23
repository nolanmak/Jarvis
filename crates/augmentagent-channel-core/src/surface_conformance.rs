//! #1288 — shared conformance fixtures for owner conversation surfaces.
//!
//! One scenario ([`native_session_conformance`]) and one offline provider
//! stand-in ([`RecordingNativeProvider`]) that every surface adapter runs
//! against: Discord's conversation path in the CLI, and Slack's harness in
//! `augmentagent-channel-slack` and in the CLI. Test support only: nothing
//! here spawns a process, touches the network or reads live state.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use async_trait::async_trait;
use tokio::sync::Notify;

use crate::native_session::{Launch, CURRENT};
use crate::providers::ProviderKind;
use crate::reasoner::{Reasoner, ReasonerOpts};

/// One provider call as the stand-in saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeCall {
    /// The provider's own session ID (created or resumed).
    pub native_session_id: String,
    /// True when the call resumed an existing session.
    pub resumed: bool,
    pub prompt: String,
    /// `ReasonerOpts.session_id`: the per-turn audit and handoff identity.
    pub audit_session_id: Option<String>,
    /// Extra environment the turn handed the agent.
    pub env: Vec<(String, String)>,
}

/// Joins [`CURRENT`] exactly like the Claude and Codex CLI adapters: begins
/// a lease, observes the session ID it creates or resumes, and finishes it.
/// Answers `session=<id> turn=<n>` where `n` counts calls in that session.
/// Prompts containing the hold marker wait for [`release`](Self::release).
pub struct RecordingNativeProvider {
    kind: ProviderKind,
    hold: Option<String>,
    calls: Mutex<Vec<NativeCall>>,
    observed: Mutex<Vec<String>>,
    dropped: AtomicUsize,
    /// Notified after each call has joined its session.
    pub started: Notify,
    pub release: Notify,
}

struct DropProbe<'a> {
    counter: &'a AtomicUsize,
    armed: bool,
}

impl Drop for DropProbe<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.counter.fetch_add(1, Ordering::SeqCst);
        }
    }
}

impl RecordingNativeProvider {
    pub fn new(kind: ProviderKind) -> Self {
        Self {
            kind,
            hold: None,
            calls: Mutex::new(Vec::new()),
            observed: Mutex::new(Vec::new()),
            dropped: AtomicUsize::new(0),
            started: Notify::new(),
            release: Notify::new(),
        }
    }

    /// Hold every call whose prompt contains `marker` until released.
    pub fn holding_on(mut self, marker: &str) -> Self {
        self.hold = Some(marker.to_string());
        self
    }

    /// Completed calls, in order.
    pub fn calls(&self) -> Vec<NativeCall> {
        self.calls.lock().unwrap().clone()
    }

    /// Session IDs observed at the start of every call, finished or not.
    pub fn observed_ids(&self) -> Vec<String> {
        self.observed.lock().unwrap().clone()
    }

    /// Calls torn down while held (cancelled or aborted mid-turn).
    pub fn dropped_mid_turn(&self) -> usize {
        self.dropped.load(Ordering::SeqCst)
    }

    /// Answer as a provider inside the current native session, recording
    /// `env` as the turn's extra environment.
    pub async fn answer(
        &self,
        opts: &ReasonerOpts,
        prompt: &str,
        env: Vec<(String, String)>,
    ) -> anyhow::Result<String> {
        let Ok(session) = CURRENT.try_with(std::sync::Arc::clone) else {
            return Ok("session=none turn=0".into());
        };
        let mut lease = session.begin(self.kind)?;
        let (id, resumed) = match lease.launch() {
            Launch::Resume { id } => (id, true),
            Launch::Create {
                requested_id: Some(id),
            } => (id, false),
            Launch::Create { requested_id: None } => {
                (format!("thread-{}", uuid::Uuid::new_v4()), false)
            }
        };
        lease.observe(&id)?;
        self.observed.lock().unwrap().push(id.clone());
        self.started.notify_one();
        if self.hold.as_deref().is_some_and(|m| prompt.contains(m)) {
            let mut probe = DropProbe {
                counter: &self.dropped,
                armed: true,
            };
            self.release.notified().await;
            probe.armed = false;
        }
        lease.finish()?;
        let mut calls = self.calls.lock().unwrap();
        calls.push(NativeCall {
            native_session_id: id.clone(),
            resumed,
            prompt: prompt.to_string(),
            audit_session_id: opts.session_id.clone(),
            env,
        });
        let n = calls.iter().filter(|c| c.native_session_id == id).count();
        Ok(format!("session={id} turn={n}"))
    }
}

#[async_trait]
impl Reasoner for RecordingNativeProvider {
    async fn call(&self, opts: &ReasonerOpts, user_message: &str) -> anyhow::Result<String> {
        self.answer(opts, user_message, crate::surface_turn::turn_env())
            .await
    }
}

/// A surface under test. Conversation and turn keys are small labels
/// (`"A"`, `"1"`); the adapter maps them onto its own identifiers.
#[async_trait]
pub trait ConformanceAdapter: Send + Sync {
    fn name(&self) -> &str;
    /// One owner message in `conversation`, delivered as `turn`.
    async fn turn(&self, conversation: &str, turn: &str, text: &str) -> anyhow::Result<String>;
    /// Drop all in-memory state as a daemon restart would, keeping the store.
    async fn restart(&self);
}

/// The shared scenario: follow-ups continue the conversation's native
/// session, other conversations get their own, a redelivered turn never runs
/// twice, continuity survives a restart, and each turn has its own audit ID.
pub async fn native_session_conformance(
    adapter: &dyn ConformanceAdapter,
    provider: &RecordingNativeProvider,
) {
    let name = adapter.name().to_string();
    let base = provider.calls().len();
    let call = |i: usize| provider.calls()[base + i].clone();

    adapter.turn("A", "1", "first message in A").await.unwrap();
    adapter.turn("A", "2", "follow-up in A").await.unwrap();
    adapter.turn("B", "1", "first message in B").await.unwrap();
    assert_eq!(
        provider.calls().len() - base,
        3,
        "{name}: one provider call per turn"
    );
    let (a1, a2, b1) = (call(0), call(1), call(2));
    assert!(!a1.resumed && a2.resumed, "{name}: the follow-up resumes");
    assert_eq!(
        a1.native_session_id, a2.native_session_id,
        "{name}: same session in A"
    );
    assert_ne!(
        a1.native_session_id, b1.native_session_id,
        "{name}: B is its own session"
    );
    assert!(a2.prompt.contains("follow-up in A"), "{name}");
    assert!(
        !a2.prompt.contains("first message in A"),
        "{name}: no replayed history"
    );

    // Redelivery of a turn that already ran.
    let _ = adapter.turn("A", "2", "follow-up in A").await;
    assert_eq!(
        provider.calls().len() - base,
        3,
        "{name}: a redelivered turn ran twice"
    );

    adapter.restart().await;
    adapter.turn("A", "3", "after restart in A").await.unwrap();
    let a3 = call(3);
    assert!(a3.resumed, "{name}: continuity across restart");
    assert_eq!(a3.native_session_id, a1.native_session_id, "{name}");

    let audit: Vec<Option<String>> = (0..4).map(|i| call(i).audit_session_id).collect();
    for (i, id) in audit.iter().enumerate() {
        let id = id.as_deref().unwrap_or("");
        assert!(
            !id.is_empty() && id != "-",
            "{name}: turn {i} has no audit id"
        );
        assert_eq!(
            audit
                .iter()
                .filter(|other| other.as_deref() == Some(id))
                .count(),
            1,
            "{name}: audit id {id} reused across turns"
        );
    }
}
