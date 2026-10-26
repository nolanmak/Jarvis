//! One native Codex or Claude conversation, shared by text and voice turns.
//! The session ID is independent of per-turn audit and handoff IDs.

use std::sync::{Arc, Mutex};

use crate::providers::ProviderKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Launch {
    Create { requested_id: Option<String> },
    Resume { id: String },
}

#[derive(Debug)]
struct State {
    id: Option<String>,
    active: bool,
    uncertain: bool,
    native_submitted_at_ms: Option<u64>,
    first_text_output_at_ms: Option<u64>,
}

#[derive(Debug)]
pub struct NativeSession {
    provider: ProviderKind,
    state: Mutex<State>,
}

pub struct Lease {
    session: Arc<NativeSession>,
    launch: Launch,
    observed_id: Option<String>,
    finished: bool,
}

tokio::task_local! { pub static CURRENT: Arc<NativeSession>; }
tokio::task_local! { pub static FRESH_REVIEW: bool; }

/// A controller-owned set of actual native conversations for one restarted
/// builder episode. Audit UUIDs are deliberately not used as CLI identities.
#[derive(Default)]
pub struct FreshBuilderSessions {
    sessions: Mutex<Vec<Arc<NativeSession>>>,
}
#[derive(Debug, Clone)]
pub struct ObservedConversation {
    pub provider: ProviderKind,
    pub id: String,
    pub uncertain: bool,
}
tokio::task_local! { static FRESH_BUILDERS: Arc<FreshBuilderSessions>; }
impl FreshBuilderSessions {
    pub fn new() -> Arc<Self> { Arc::new(Self::default()) }
    pub async fn scope<T>(self: &Arc<Self>, call: impl std::future::Future<Output = T>) -> T {
        FRESH_BUILDERS.scope(Arc::clone(self), call).await
    }
    pub fn observed(&self) -> Vec<ObservedConversation> {
        self.sessions.lock().expect("builder sessions mutex poisoned").iter().filter_map(|session| {
            session.id().map(|id| ObservedConversation { provider: session.provider(), id, uncertain: session.is_uncertain() })
        }).collect()
    }
    fn session(&self, provider: ProviderKind) -> anyhow::Result<Option<Arc<NativeSession>>> {
        if !matches!(provider, ProviderKind::Claude | ProviderKind::Codex) { return Ok(None); }
        let mut sessions = self.sessions.lock().expect("builder sessions mutex poisoned");
        if let Some(session) = sessions.iter().find(|session| session.provider() == provider) {
            return Ok(Some(Arc::clone(session)));
        }
        let session = NativeSession::new(provider)?;
        sessions.push(Arc::clone(&session));
        Ok(Some(session))
    }
}

/// Actual transport selection happens after fallback chooses a provider.
/// A nested independent review must override even a fresh builder episode.
/// Transport adapters use this protocol handle; observing an ID here alone is
/// not evidence that a real provider CLI executed.
pub fn current_for(provider: ProviderKind) -> anyhow::Result<Option<Arc<NativeSession>>> {
    if !is_fresh_review() {
        if let Ok(session) = FRESH_BUILDERS.try_with(|sessions| sessions.session(provider)) {
            return session;
        }
    }
    Ok(CURRENT.try_with(Arc::clone).ok())
}

/// Override any inherited builder conversation. Each invocation creates a new
/// native conversation and returns the identifier actually observed from the CLI.
pub async fn fresh_review_scope<T>(
    provider: ProviderKind,
    call: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<(T, String)> {
    let session = NativeSession::new(provider)?;
    let result = FRESH_REVIEW.scope(true, CURRENT.scope(session.clone(), call)).await?;
    let id = session.id().ok_or_else(|| anyhow::anyhow!("fresh review did not observe a native conversation identifier"))?;
    anyhow::ensure!(!session.is_uncertain(), "fresh reviewer conversation is uncertain");
    Ok((result, id))
}

pub fn is_fresh_review() -> bool {
    FRESH_REVIEW.try_with(|fresh| *fresh).unwrap_or(false)
}


impl NativeSession {
    pub fn new(provider: ProviderKind) -> anyhow::Result<Arc<Self>> {
        Self::from_id(provider, None)
    }

    pub fn from_id(provider: ProviderKind, id: Option<String>) -> anyhow::Result<Arc<Self>> {
        anyhow::ensure!(matches!(provider, ProviderKind::Claude | ProviderKind::Codex),
            "native conversation requires Claude or Codex");
        anyhow::ensure!(id.as_ref().is_none_or(|value| !value.trim().is_empty()),
            "native session ID is empty");
        Ok(Arc::new(Self {
            provider,
            state: Mutex::new(State { id, active: false, uncertain: false,
                native_submitted_at_ms: None, first_text_output_at_ms: None }),
        }))
    }

    pub fn begin(self: &Arc<Self>, provider: ProviderKind) -> anyhow::Result<Lease> {
        anyhow::ensure!(provider == self.provider, "native session provider mismatch");
        let mut state = self.state.lock().expect("native session mutex poisoned");
        anyhow::ensure!(!state.uncertain, "native session has an uncertain in-flight turn");
        anyhow::ensure!(!state.active, "native session already has an active writer");
        let launch = match &state.id {
            Some(id) => Launch::Resume { id: id.clone() },
            None if provider == ProviderKind::Claude => Launch::Create {
                requested_id: Some(uuid::Uuid::new_v4().to_string()),
            },
            None => Launch::Create { requested_id: None },
        };
        anyhow::ensure!(!is_fresh_review() || matches!(launch, Launch::Create { .. }),
            "fresh review cannot resume an existing native conversation");
        state.active = true;
        state.native_submitted_at_ms = None;
        state.first_text_output_at_ms = None;
        Ok(Lease { session: Arc::clone(self), launch, observed_id: None, finished: false })
    }

    pub fn id(&self) -> Option<String> {
        self.state.lock().expect("native session mutex poisoned").id.clone()
    }

    pub fn is_uncertain(&self) -> bool {
        self.state.lock().expect("native session mutex poisoned").uncertain
    }

    pub fn provider(&self) -> ProviderKind {
        self.provider
    }

    pub fn first_text_output_at_ms(&self) -> Option<u64> {
        self.state.lock().expect("native session mutex poisoned").first_text_output_at_ms
    }

    pub fn native_submitted_at_ms(&self) -> Option<u64> {
        self.state.lock().expect("native session mutex poisoned").native_submitted_at_ms
    }
}

impl Lease {
    pub fn launch(&self) -> Launch {
        self.launch.clone()
    }

    /// Record when the prompt was fully written to the native CLI.
    pub fn observe_native_submission(&self) {
        let mut state = self.session.state.lock().expect("native session mutex poisoned");
        if state.active && state.native_submitted_at_ms.is_none() {
            state.native_submitted_at_ms = Some(std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).unwrap_or_default()
                .as_millis().try_into().unwrap_or(u64::MAX));
        }
    }

    /// Record the first nonempty assistant text observed in this native turn.
    pub fn observe_first_text_output(&self) {
        let mut state = self.session.state.lock().expect("native session mutex poisoned");
        if state.active && state.first_text_output_at_ms.is_none() {
            state.first_text_output_at_ms = Some(std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).unwrap_or_default()
                .as_millis().try_into().unwrap_or(u64::MAX));
        }
    }

    /// Verify the native CLI's own reported identity, not a derived audit ID.
    pub fn observe(&mut self, id: &str) -> anyhow::Result<()> {
        anyhow::ensure!(!id.trim().is_empty(), "native CLI returned no session ID");
        match &self.launch {
            Launch::Resume { id: expected } =>
                anyhow::ensure!(id == expected, "native CLI resumed a different session"),
            Launch::Create { requested_id: Some(expected) } =>
                anyhow::ensure!(id == expected, "native CLI created a different session"),
            Launch::Create { requested_id: None } => {}
        }
        if let Some(observed) = &self.observed_id {
            anyhow::ensure!(observed == id, "native CLI changed session ID mid-turn");
        }
        let mut state = self.session.state.lock().expect("native session mutex poisoned");
        if let Some(existing) = &state.id {
            anyhow::ensure!(existing == id, "native session identity changed");
        }
        state.id = Some(id.to_owned());
        self.observed_id = Some(id.to_owned());
        Ok(())
    }

    /// Only a successful completed native turn may clear the active writer.
    pub fn finish(mut self) -> anyhow::Result<String> {
        let id = self.observed_id.as_ref()
            .ok_or_else(|| anyhow::anyhow!("native CLI did not report a session ID"))?
            .clone();
        let mut state = self.session.state.lock().expect("native session mutex poisoned");
        if let Some(existing) = &state.id {
            anyhow::ensure!(existing == &id, "native session identity changed");
        }
        state.id = Some(id.clone());
        state.active = false;
        self.finished = true;
        Ok(id)
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if !self.finished {
            let mut state = self.session.state.lock().expect("native session mutex poisoned");
            state.active = false;
            state.uncertain = true;
        }
    }
}

#[cfg(test)]
mod fresh_review_tests {
    use super::*;

    #[tokio::test]
    async fn fresh_review_overrides_builder_and_cannot_resume_even_on_retry() {
        let builder = NativeSession::from_id(ProviderKind::Codex, Some("actual-builder".into())).unwrap();
        let (_, observed) = CURRENT.scope(builder.clone(), fresh_review_scope(ProviderKind::Codex, async {
            let session = CURRENT.with(Arc::clone);
            let mut lease = session.begin(ProviderKind::Codex)?;
            assert!(matches!(lease.launch(), Launch::Create { .. }));
            lease.observe("actual-review")?;
            lease.finish()?;
            assert!(session.begin(ProviderKind::Codex).is_err(), "retry must not resume reviewer history");
            Ok(())
        })).await.unwrap();
        assert_eq!(observed, "actual-review");
        assert_eq!(builder.id().as_deref(), Some("actual-builder"));
        assert!(!builder.is_uncertain());
    }

    #[tokio::test]
    async fn restarted_builder_is_provider_aware_reusable_and_does_not_leak_into_review() {
        let inherited = NativeSession::from_id(ProviderKind::Claude, Some("old-builder".into())).unwrap();
        let episode = FreshBuilderSessions::new();
        CURRENT.scope(inherited.clone(), episode.scope(async {
            for provider in [ProviderKind::Claude, ProviderKind::Codex] {
                let session = current_for(provider).unwrap().unwrap();
                let mut lease = session.begin(provider).unwrap();
                let id = match lease.launch() {
                    Launch::Create { requested_id } => requested_id.unwrap_or("new-codex".into()),
                    _ => panic!("restarted builder inherited history"),
                };
                lease.observe(&id).unwrap();
                lease.finish().unwrap();
            }
            fresh_review_scope(ProviderKind::Codex, async {
                let session = current_for(ProviderKind::Codex)?.unwrap();
                let mut lease = session.begin(ProviderKind::Codex)?;
                assert!(matches!(lease.launch(), Launch::Create { .. }));
                lease.observe("independent-review")?;
                lease.finish()?;
                Ok(())
            }).await.unwrap();
        })).await;
        episode.scope(async {
            let session = current_for(ProviderKind::Codex).unwrap().unwrap();
            let mut lease = session.begin(ProviderKind::Codex).unwrap();
            assert_eq!(lease.launch(), Launch::Resume { id: "new-codex".into() });
            lease.observe("new-codex").unwrap();
            lease.finish().unwrap();
        }).await;
        assert_eq!(inherited.id().as_deref(), Some("old-builder"));
        let observed = episode.observed();
        assert_eq!(observed.len(), 2);
        assert!(observed.iter().all(|s| !s.uncertain && s.id != "old-builder" && s.id != "independent-review"));
    }

    #[tokio::test]
    async fn fresh_review_requires_cli_observed_identity() {
        assert!(fresh_review_scope(ProviderKind::Codex, async { Ok("LGTM") }).await.is_err());
    }
}
