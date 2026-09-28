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
