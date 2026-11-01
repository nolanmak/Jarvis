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
    /// #1626 — set by the active lease the moment it starts writing the
    /// prompt to the native CLI. Before that point nothing has reached the
    /// CLI, so a dropped lease has no native side effect to be uncertain of.
    submission_started: bool,
    native_submitted_at_ms: Option<u64>,
    first_text_output_at_ms: Option<u64>,
}

#[derive(Debug)]
pub struct NativeSession {
    provider: ProviderKind,
    state: Mutex<State>,
    retired_vm: Mutex<Vec<crate::native_vm::Retirement>>,
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
    retain_runtime: bool,
    workflow_owner: Option<crate::native_vm::RuntimeOwner>,
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
    /// The durable workflow registration, rather than this controller scope,
    /// owns retirement of these builder conversations.
    pub fn for_workflow(owner: crate::native_vm::RuntimeOwner) -> anyhow::Result<Arc<Self>> {
        anyhow::ensure!(!owner.workflow_id.is_empty() && owner.lease_generation > 0, "invalid workflow builder owner");
        Ok(Arc::new(Self { sessions:Mutex::new(Vec::new()), retain_runtime:true, workflow_owner:Some(owner) }))
    }
    pub async fn scope<T>(self: &Arc<Self>, call: impl std::future::Future<Output = T>) -> T {
        FRESH_BUILDERS.scope(Arc::clone(self), call).await
    }
    /// Restored workflow conversations retain their durable workflow owner,
    /// including when no provider conversation has been observed yet. Native
    /// preparation still checks that owner against the current audit fence.
    pub fn from_observed(owner:crate::native_vm::RuntimeOwner,observed:&[ObservedConversation])->anyhow::Result<Arc<Self>> {
        let builders=Self::for_workflow(owner)?;
        let mut restored:Vec<Arc<NativeSession>>=Vec::new();
        for observation in observed {
            anyhow::ensure!(!observation.uncertain,"uncertain builder conversation requires reconciliation");
            anyhow::ensure!(!observation.id.is_empty() && observation.id.len()<=512 && observation.id.bytes().all(|b|b.is_ascii_graphic()),"invalid native conversation identity");
            anyhow::ensure!(!restored.iter().any(|s|s.provider()==observation.provider),"duplicate builder provider conversation");
            restored.push(NativeSession::from_id(observation.provider,Some(observation.id.clone()))?);
        }
        *builders.sessions.lock().expect("builder sessions mutex poisoned")=restored;
        Ok(builders)
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

pub(crate) fn current_builder_owner() -> Option<crate::native_vm::RuntimeOwner> {
    if is_fresh_review() { return None; }
    FRESH_BUILDERS.try_with(|builders| builders.workflow_owner.clone()).ok().flatten()
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
            retired_vm: Mutex::new(Vec::new()),
            state: Mutex::new(State { id, active: false, uncertain: false, submission_started: false,
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
        state.submission_started = false;
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

impl Drop for NativeSession {
    fn drop(&mut self) {
        if let Ok(paths)=self.retired_vm.lock() { for path in paths.iter() { crate::native_vm::retire(path); } }
    }
}
impl Lease {
    pub(crate) fn register_vm_retirement(&self, path:crate::native_vm::Retirement) {
        if is_fresh_review() || FRESH_BUILDERS.try_with(|builders|!builders.retain_runtime).unwrap_or(false) {
            let mut paths=self.session.retired_vm.lock().expect("native retirement mutex poisoned");
            if !paths.contains(&path) { paths.push(path); }
        }
    }
    pub fn launch(&self) -> Launch {
        self.launch.clone()
    }

    /// #1626 — the prompt is about to be written to the native CLI. From
    /// here on the CLI may have received (part of) the turn, so a dropped
    /// lease leaves the session uncertain. Call before the first byte.
    pub fn mark_submitting(&self) {
        let mut state = self.session.state.lock().expect("native session mutex poisoned");
        if state.active {
            state.submission_started = true;
        }
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
            // #1626 — a refusal before the prompt reached the CLI (resource
            // capacity, hook preparation, spawn failure) ran nothing on the
            // native side; the session stays clean and the next turn resumes
            // it. Once submission started, or the CLI reported a session, the
            // turn may have run and replay is blocked until inspected.
            if state.submission_started || self.observed_id.is_some() {
                state.uncertain = true;
            }
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

#[cfg(test)]
mod lease_drop_tests {
    //! #1626 — only a turn the native CLI may have received leaves the
    //! session uncertain. A lease dropped by a pre-spawn refusal (resource
    //! capacity, hook preparation, spawn failure) had no native side effect
    //! and must not latch the conversation.
    use super::*;

    #[test]
    fn a_lease_dropped_before_submission_leaves_the_session_clean_and_resumable() {
        let session = NativeSession::from_id(ProviderKind::Claude, Some("bound".into())).unwrap();
        {
            let lease = session.begin(ProviderKind::Claude).unwrap();
            assert_eq!(lease.launch(), Launch::Resume { id: "bound".into() });
            // Refused before any byte reached the CLI: dropped unfinished.
        }
        assert!(!session.is_uncertain(), "no prompt was submitted, nothing is uncertain");
        let mut again = session.begin(ProviderKind::Claude).expect("writer released");
        assert_eq!(again.launch(), Launch::Resume { id: "bound".into() });
        again.observe("bound").unwrap();
        again.finish().unwrap();
        assert!(!session.is_uncertain());
    }

    #[test]
    fn a_lease_dropped_after_submission_started_marks_the_session_uncertain() {
        let session = NativeSession::from_id(ProviderKind::Codex, Some("bound".into())).unwrap();
        {
            let lease = session.begin(ProviderKind::Codex).unwrap();
            lease.mark_submitting();
            // The process died with the prompt partly or fully written.
        }
        assert!(session.is_uncertain(), "the CLI may have run the turn");
        let blocked = session.begin(ProviderKind::Codex).err().expect("replay must be blocked");
        assert!(blocked.to_string().contains("uncertain"), "{blocked}");
    }

    #[test]
    fn a_lease_dropped_after_the_cli_reported_a_session_is_uncertain_even_without_submission() {
        let session = NativeSession::new(ProviderKind::Claude).unwrap();
        {
            let mut lease = session.begin(ProviderKind::Claude).unwrap();
            let Launch::Create { requested_id: Some(id) } = lease.launch() else { panic!("claude creates with an id") };
            lease.observe(&id).unwrap();
        }
        assert!(session.is_uncertain());
    }

    #[test]
    fn a_new_lease_starts_without_a_stale_submission_mark() {
        let session = NativeSession::from_id(ProviderKind::Claude, Some("bound".into())).unwrap();
        {
            let mut lease = session.begin(ProviderKind::Claude).unwrap();
            lease.mark_submitting();
            lease.observe("bound").unwrap();
            lease.finish().unwrap();
        }
        {
            let _lease = session.begin(ProviderKind::Claude).unwrap();
        }
        assert!(!session.is_uncertain(), "the mark belongs to the finished turn, not the next lease");
    }
}

#[cfg(test)]
mod builder_restore_tests {
    use super::*;
    fn fixture_owner()->crate::native_vm::RuntimeOwner {
        crate::native_vm::RuntimeOwner {workflow_id:"fixture".into(),lease_generation:1}
    }
    fn runtime_fixture(root:&std::path::Path,name:&str)->std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(root,std::fs::Permissions::from_mode(0o700)).unwrap();
        crate::native_vm::initialize_native_test_store(root);
        let path=root.join("codex").join(name);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("owned.json"),r#"{"version":1,"owner":{"workflow_id":"fixture","lease_generation":1},"audit_path":"/audit/fixture"}"#).unwrap();
        std::fs::set_permissions(path.join("owned.json"),std::fs::Permissions::from_mode(0o600)).unwrap();
        path
    }
    fn finish_native_turn(provider:ProviderKind,runtime:&std::path::Path)->String {
        let session=current_for(provider).unwrap().unwrap();
        let mut lease=session.begin(provider).unwrap();
        lease.register_vm_retirement(crate::native_vm::test_retirement(runtime.to_owned(),crate::native_vm::ownership::Authority{owner:crate::native_vm::RuntimeOwner{workflow_id:"fixture".into(),lease_generation:1},audit_path:"/audit/fixture".into()}));
        let id=match lease.launch() {
            Launch::Resume {id}|Launch::Create {requested_id:Some(id)}=>id,
            Launch::Create {requested_id:None}=>"observed-native-codex".into(),
        };
        lease.observe(&id).unwrap();lease.finish().unwrap()
    }
    #[tokio::test]
    async fn restored_owner_builders_keep_runtime_after_controller_exit_and_resume_again() {
        for provider in [ProviderKind::Codex,ProviderKind::Claude] {
            for existing in [false,true] {
                let root=tempfile::tempdir().unwrap();let runtime=runtime_fixture(root.path(),"session-owner");
                let recorded=if existing {vec![ObservedConversation {provider,id:"previous-native-session".into(),uncertain:false}]} else {vec![]};
                let sessions=FreshBuilderSessions::from_observed(fixture_owner(),&recorded).unwrap();
                let id=sessions.scope(async {finish_native_turn(provider,&runtime)}).await;
                let observed=sessions.observed();assert_eq!(observed.len(),1);drop(sessions);
                assert!(!runtime.join("retired.json").exists(),"saved native conversation was retired when its controller exited: {provider:?}, existing={existing}");
                let restored=FreshBuilderSessions::from_observed(fixture_owner(),&observed).unwrap();
                let resumed=restored.scope(async {
                    let session=current_for(provider).unwrap().unwrap();
                    assert_eq!(session.begin(provider).unwrap().launch(),Launch::Resume {id:id.clone()});
                    finish_native_turn(provider,&runtime)
                }).await;
                assert_eq!(resumed,id);drop(restored);
                assert!(!runtime.join("retired.json").exists());
            }
        }
    }
    #[tokio::test]
    async fn first_workflow_builder_keeps_runtime_after_controller_exit() {
        for provider in [ProviderKind::Codex, ProviderKind::Claude] {
            let root=tempfile::tempdir().unwrap();
            let runtime=runtime_fixture(root.path(),"session-first-workflow-builder");
            let sessions=FreshBuilderSessions::for_workflow(crate::native_vm::RuntimeOwner {workflow_id:"fixture".into(),lease_generation:1}).unwrap();
            let id=sessions.scope(async {finish_native_turn(provider,&runtime)}).await;
            let observed=sessions.observed();
            assert_eq!(observed.len(),1);
            drop(sessions);
            assert!(!runtime.join("retired.json").exists(),"first builder conversation retired before workflow handoff: {provider:?}");
            let restored=FreshBuilderSessions::from_observed(fixture_owner(),&observed).unwrap();
            restored.scope(async {
                let session=current_for(provider).unwrap().unwrap();
                assert_eq!(session.begin(provider).unwrap().launch(),Launch::Resume {id});
            }).await;
            drop(restored);
            assert!(!runtime.join("retired.json").exists());
        }
    }
    #[tokio::test]
    async fn first_workflow_builder_does_not_retain_an_independent_reviewer() {
        let root=tempfile::tempdir().unwrap();
        let runtime=runtime_fixture(root.path(),"session-workflow-reviewer");
        let sessions=FreshBuilderSessions::for_workflow(crate::native_vm::RuntimeOwner {workflow_id:"fixture".into(),lease_generation:1}).unwrap();
        sessions.scope(async {
            fresh_review_scope(ProviderKind::Claude,async {
                finish_native_turn(ProviderKind::Claude,&runtime);Ok(())
            }).await.unwrap();
        }).await;
        drop(sessions);
        assert!(runtime.join("retired.json").exists());
    }
    #[tokio::test]
    async fn workflow_builder_owner_context_is_separate_from_fresh_review() {
        let owner=crate::native_vm::RuntimeOwner {workflow_id:"fixture".into(),lease_generation:1};
        let sessions=FreshBuilderSessions::for_workflow(owner.clone()).unwrap();
        sessions.scope(async {
            assert_eq!(current_builder_owner(),Some(owner.clone()));
            fresh_review_scope(ProviderKind::Codex,async {
                assert_eq!(current_builder_owner(),None);
                let session=current_for(ProviderKind::Codex)?.unwrap();
                let mut lease=session.begin(ProviderKind::Codex)?;
                lease.observe("independent-review")?;lease.finish()?;Ok(())
            }).await.unwrap();
            assert_eq!(current_builder_owner(),Some(owner));
        }).await;
    }
    #[test]
    fn workflow_builder_owner_must_be_a_real_workflow_generation() {
        for (workflow,generation) in [("",1),("fixture",0),("fixture",-1)] {
            let owner=crate::native_vm::RuntimeOwner {workflow_id:workflow.into(),lease_generation:generation};
            assert!(FreshBuilderSessions::for_workflow(owner.clone()).is_err());
            assert!(FreshBuilderSessions::from_observed(owner,&[]).is_err());
        }
    }
    #[tokio::test]
    async fn workflow_builder_cannot_prepare_a_foreign_audit_owner() {
        const CHILD:&str="JARVIS_TEST_BUILDER_OWNER_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let child=std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact","native_session::builder_restore_tests::workflow_builder_cannot_prepare_a_foreign_audit_owner","--test-threads=1"])
                .env(CHILD,"1").env_remove("AUGMENTAGENT_NATIVE_VM_POLICY").output().unwrap();
            assert!(child.status.success(),"owner mismatch was not fenced before policy selection: {} {}",String::from_utf8_lossy(&child.stdout),String::from_utf8_lossy(&child.stderr));
            return;
        }
        let root=tempfile::tempdir().unwrap();
        for name in ["logs","anchors"] {std::fs::create_dir(root.path().join(name)).unwrap();}
        let logger=Arc::new(crate::tool_audit::AuditLogger::protected(root.path().join("logs/model.jsonl"),root.path().join("anchors/model.json"),&[]).unwrap());
        logger.set_native_runtime_owner(crate::native_vm::RuntimeOwner {workflow_id:"other-workflow".into(),lease_generation:1}).unwrap();
        let mut opts=crate::reasoner::triage_opts(None);
        opts.audit_logger=Some(logger);
        let owner=crate::native_vm::RuntimeOwner {workflow_id:"fixture".into(),lease_generation:1};
        for sessions in [FreshBuilderSessions::for_workflow(owner).unwrap(),FreshBuilderSessions::from_observed(fixture_owner(),&[]).unwrap()] {
            sessions.scope(async {
                let command=tokio::process::Command::new("/usr/bin/false");
                let error=crate::native_vm::Launch::prepare("codex",&opts,&command,None,None).err().expect("foreign audit owner was accepted");
                assert!(error.to_string().contains("native builder scope owner mismatch"),"wrong owner reached native preparation: {error:#}");
            }).await;
        }
    }
    #[tokio::test]
    async fn scoped_builders_still_retire_their_completed_runtime() {
        let root=tempfile::tempdir().unwrap();let runtime=runtime_fixture(root.path(),"session-scoped");
        let sessions=FreshBuilderSessions::new();
        sessions.scope(async {finish_native_turn(ProviderKind::Codex,&runtime)}).await;drop(sessions);
        assert!(runtime.join("retired.json").exists());
    }
    #[tokio::test]
    async fn independent_reviews_retire_even_inside_a_restored_builder() {
        let root=tempfile::tempdir().unwrap();let runtime=runtime_fixture(root.path(),"session-reviewer");
        let sessions=FreshBuilderSessions::from_observed(fixture_owner(),&[]).unwrap();
        sessions.scope(async {
            fresh_review_scope(ProviderKind::Claude,async {
                finish_native_turn(ProviderKind::Claude,&runtime);Ok(())
            }).await.unwrap();
        }).await;drop(sessions);
        assert!(runtime.join("retired.json").exists(),"reviewer runtime outlived its independent scope");
    }
    #[tokio::test]
    async fn restored_builder_sessions_resume_their_provider_and_independent_review_stays_fresh() {
        for provider in [ProviderKind::Codex,ProviderKind::Claude] {
            let recorded=ObservedConversation {provider,id:format!("recorded-{}",provider.name()),uncertain:false};
            let sessions=FreshBuilderSessions::from_observed(fixture_owner(),&[recorded.clone()]).expect("persisted native conversations cannot be restored");
            sessions.scope(async {
                let session=current_for(provider).unwrap().unwrap();let mut lease=session.begin(provider).unwrap();
                assert_eq!(lease.launch(),Launch::Resume {id:recorded.id.clone()});lease.observe(&recorded.id).unwrap();lease.finish().unwrap();
                let (_,id)=fresh_review_scope(provider,async {
                    let session=current_for(provider)?.unwrap();let mut lease=session.begin(provider)?;
                    let id=match lease.launch() {Launch::Create {requested_id:Some(id)}=>id,Launch::Create {requested_id:None}=>"independent-review".into(),_=>panic!("review reused a builder conversation")};lease.observe(&id)?;lease.finish()?;Ok(())
                }).await.unwrap();assert_ne!(id,recorded.id);
            }).await;
        }
    }
    #[test]
    fn uncertain_duplicate_and_invalid_restorations_are_refused() {
        let mut observation=ObservedConversation {provider:ProviderKind::Codex,id:"native-one".into(),uncertain:true};
        assert!(FreshBuilderSessions::from_observed(fixture_owner(),&[observation.clone()]).is_err());observation.uncertain=false;
        assert!(FreshBuilderSessions::from_observed(fixture_owner(),&[observation.clone(),observation.clone()]).is_err());
        observation.id="native id with whitespace".into();assert!(FreshBuilderSessions::from_observed(fixture_owner(),&[observation]).is_err());
    }

}
