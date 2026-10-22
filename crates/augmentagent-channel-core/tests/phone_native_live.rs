//! Opt-in paid native CLI probes using fake telephony and synthetic files only.
//! These do not qualify Twilio ingress, audible latency, or approval gating.
use async_trait::async_trait;
use augmentagent_approval_discord::{AuditCtx, QueryHandler};
use augmentagent_channel_core::{
    codex::CodexCliReasoner,
    native_session::CURRENT,
    phone_agent::{attach_tools, ConversationIo, PhoneAgent},
    providers::{ModelTier, ProviderKind},
    surface_turn::{run_surface_turn, SurfaceTurnOutcome, SurfaceTurnRequest},
    ClaudeCliReasoner, Reasoner, ReasonerOpts,
};
use augmentagent_phone::{
    tools::ToolServer, Clock, DialRequest, OwnerLink, PhoneConfig, PhoneService, ProviderError,
    SystemClock, Telephony,
};
use augmentagent_store::{
    owner::ControlConversationKind,
    phone::{CallState, PhoneOrigin},
    Store, SurfaceAccountRef, SurfaceConversationRef, SurfaceOwnerRef, SurfacePlatform,
    SurfaceTurnRef,
};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct FakePhone {
    creates: AtomicUsize,
}
#[async_trait]
impl Telephony for FakePhone {
    async fn create(&self, _: DialRequest) -> Result<String, ProviderError> {
        self.creates.fetch_add(1, Ordering::SeqCst);
        Ok("CA-synthetic".into())
    }
    async fn hangup(&self, _: &str) -> Result<(), ProviderError> {
        Ok(())
    }
}
struct NativeQuery {
    provider: Box<dyn Reasoner>,
    root: PathBuf,
    observed: Mutex<Vec<String>>,
    tools: Arc<ToolServer>,
    bin: PathBuf,
}
#[async_trait]
impl QueryHandler for NativeQuery {
    async fn answer(&self, ctx: &AuditCtx, question: &str) -> anyhow::Result<String> {
        let mut opts = ReasonerOpts::pinned(ModelTier::Fast,
            "Read the requested synthetic file anew on every turn. Reply with its exact current content. When explicitly requested, use the bound phone tool to call the owner, and report the call ID. All phone effects in this probe are synthetic.");
        opts.cwd = Some(self.root.clone());
        opts.add_dirs = vec![self.root.clone()];
        opts.allowed_tools = vec!["Read".into()];
        opts.session_id = Some(ctx.session_id.clone());
        let _grant = attach_tools(&mut opts, &self.tools, &self.bin)?
            .expect("authenticated probe must expose phone tools");
        let answer = self.provider.call(&opts, question).await?;
        let id = CURRENT
            .try_with(|session| session.id())?
            .expect("native session observed");
        self.observed.lock().unwrap().push(id);
        Ok(answer)
    }
}
#[derive(Default)]
struct Io {
    mirrors: Mutex<Vec<String>>,
}
#[async_trait]
impl ConversationIo for Io {
    async fn history(&self, _: &PhoneOrigin) -> anyhow::Result<String> {
        Ok(String::new())
    }
    async fn mirror(&self, _: &PhoneOrigin, _: &str, text: &str) -> anyhow::Result<()> {
        self.mirrors.lock().unwrap().push(text.into());
        Ok(())
    }
    fn audit_context(&self, _: &PhoneOrigin, turn_id: &str) -> AuditCtx {
        let mut ctx = AuditCtx::empty();
        ctx.owner_authorized = true;
        ctx.session_id = turn_id.into();
        ctx
    }
}
async fn exercise(kind: ProviderKind) {
    let bin = PathBuf::from(
        std::env::var_os("AUGMENTAGENT_PHONE_PROBE_BIN")
            .expect("set AUGMENTAGENT_PHONE_PROBE_BIN to the freshly built augmentagent binary"),
    )
    .canonicalize()
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("state.db")).unwrap());
    let account =
        SurfaceAccountRef::new(SurfacePlatform::new("slack").unwrap(), "synthetic-team").unwrap();
    let owner = SurfaceOwnerRef::new(account.clone(), "synthetic-owner").unwrap();
    let chat = SurfaceConversationRef::new(account, "synthetic-dm", None).unwrap();
    store.bind_surface_owner(&owner, 0).unwrap();
    store
        .set_surface_control_conversation(&chat, ControlConversationKind::Direct, 0)
        .unwrap();
    store
        .phone_import_destination("owner", "+12025550123", 0)
        .unwrap();
    let provider = Arc::new(FakePhone::default());
    let service = Arc::new(PhoneService::new(
        store.clone(),
        provider.clone(),
        Arc::new(SystemClock),
        PhoneConfig::new(
            true,
            "https://phone.example.test",
            "synthetic-verification-key-32bytes",
            vec![OwnerLink {
                surface_owner: owner.clone(),
                owner_id: "owner".into(),
            }],
        )
        .unwrap(),
    ));
    let tools = ToolServer::start(service.clone()).await.unwrap();
    let agent = Arc::new(NativeQuery {
        provider: match kind {
            ProviderKind::Claude => Box::new(ClaudeCliReasoner::new()),
            ProviderKind::Codex => Box::new(CodexCliReasoner::openai()),
            _ => unreachable!(),
        },
        root: dir.path().into(),
        observed: Mutex::new(Vec::new()),
        tools,
        bin,
    });
    let origin = PhoneOrigin {
        owner: owner.clone(),
        conversation: chat.clone(),
    };
    let io = Arc::new(Io::default());
    let bridge = PhoneAgent::new(
        store.clone(),
        service.clone(),
        agent.clone(),
        io.clone(),
        dir.path().into(),
        Arc::new(move |_| Ok(Some(kind))),
    );
    let question = "Use Read to read marker.txt anew and reply with its current content.";
    let mut call = None;
    for index in 0..5 {
        // The new value is never supplied in the prompt/history: a correct
        // response requires reading the file again, not recalling a prior turn.
        let marker = format!("SYNTHETIC_PHONE_READ_{}", uuid::Uuid::new_v4());
        std::fs::write(dir.path().join("marker.txt"), &marker).unwrap();
        let turn_id = format!("synthetic-turn-{index}");
        let prompt = if index == 0 {
            format!("{question} Also call me now using call_owner with request_id synthetic-call. Report the returned call ID as well as the file content.")
        } else {
            question.to_owned()
        };
        let answer = if index == 0 || index == 4 {
            let turn = SurfaceTurnRef::new(chat.clone(), &turn_id).unwrap();
            let ctx = io.audit_context(&origin, &turn_id);
            let cwd = dir.path().to_string_lossy();
            let query = agent.clone();
            match augmentagent_phone::context::scope(
                origin.clone(),
                None,
                run_surface_turn(
                    &store,
                    SurfaceTurnRequest {
                        turn: &turn,
                        history: "",
                        current: &prompt,
                        cwd: &cwd,
                    },
                    || Ok(Some(kind)),
                    None,
                    |prompt| async move { query.answer(&ctx, &prompt).await },
                ),
            )
            .await
            .unwrap()
            {
                SurfaceTurnOutcome::Answered(answer) => answer,
                SurfaceTurnOutcome::Cancelled => panic!("synthetic turn cancelled"),
            }
        } else {
            bridge
                .run(
                    call.as_ref().unwrap(),
                    &turn_id,
                    question,
                    None,
                    CancellationToken::new(),
                )
                .await
                .unwrap()
        };
        assert!(
            answer.contains(&marker),
            "native read did not return the current synthetic marker: {answer}"
        );
        if index == 0 {
            let created = store
                .phone_latest_call("owner")
                .unwrap()
                .expect("native CLI must invoke call_owner through MCP");
            assert_eq!(created.request_id, "synthetic-call");
            assert_eq!(created.provider_sid.as_deref(), Some("CA-synthetic"));
            assert!(
                answer.contains(&created.id),
                "native reply must include the tool's call ID"
            );
            let id = created.id.as_str();
            let now = SystemClock.now_ms();
            store
                .phone_provider_event(id, "CA-synthetic", CallState::AwaitingAcceptance, now)
                .unwrap();
            store.phone_accept(id, "CA-synthetic", now).unwrap();
            call = Some(store.phone_call("owner", id).unwrap());
        } else if index == 3 {
            let grant = service.authorize(&owner, &chat).unwrap();
            service
                .tool(
                    &grant,
                    "phone_hangup",
                    serde_json::json!({"call_id":call.as_ref().unwrap().id}),
                )
                .await
                .unwrap();
        }
    }
    let ids = agent.observed.lock().unwrap();
    assert_eq!(ids.len(), 5);
    assert!(ids.iter().all(|id| id == &ids[0]));
    assert_eq!(
        store
            .surface_conversation(&chat)
            .unwrap()
            .unwrap()
            .native_session_id,
        ids[0]
    );
    assert_eq!(io.mirrors.lock().unwrap().len(), 6);
    assert_eq!(provider.creates.load(Ordering::SeqCst), 1);
    assert!(store
        .phone_call("owner", &call.unwrap().id)
        .unwrap()
        .state
        .terminal());
}
#[tokio::test]
#[ignore = "paid native CLI probe; fake telephony, disposable session, synthetic file"]
async fn claude_text_phone_text_executes_reads_in_same_native_session() {
    exercise(ProviderKind::Claude).await;
}
#[tokio::test]
#[ignore = "paid native CLI probe; fake telephony, disposable session, synthetic file"]
async fn codex_text_phone_text_executes_reads_in_same_native_session() {
    exercise(ProviderKind::Codex).await;
}
