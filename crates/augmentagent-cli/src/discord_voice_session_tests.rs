use super::*;
use augmentagent_channel_core::{
    native_session::{Launch, CURRENT},
    providers::ProviderKind,
    Reasoner, ReasonerOpts,
};
use std::sync::Mutex;

#[test]
fn voice_socket_defaults_to_private_user_runtime_and_requires_absolute_path() {
    assert_eq!(resolve_discord_voice_socket(false, None, None).unwrap(), None);
    assert_eq!(resolve_discord_voice_socket(true, None, Some("/run/user/1000".into())).unwrap(),
        Some(PathBuf::from("/run/user/1000/augmentagent/discord-voice.sock")));
    assert_eq!(resolve_discord_voice_socket(true, Some("/private/voice.sock".into()),
        Some("/run/user/1000".into())).unwrap(), Some(PathBuf::from("/private/voice.sock")));
    assert!(resolve_discord_voice_socket(true, None, None).is_err());
    assert!(resolve_discord_voice_socket(true, Some("relative.sock".into()), None).is_err());
}

struct McpCaptureFixture(Arc<Mutex<Vec<(Option<serde_json::Value>, Vec<String>)>>>);

#[async_trait]
impl Reasoner for McpCaptureFixture {
    async fn call(&self, opts: &ReasonerOpts, _prompt: &str) -> anyhow::Result<String> {
        self.0.lock().unwrap().push((opts.settings_json.as_deref()
            .map(serde_json::from_str).transpose()?, opts.allowed_tools.clone()));
        Ok("synthetic reply".into())
    }
}

#[tokio::test]
async fn active_voice_binding_injects_tool_only_into_its_owner_conversation_turn() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("sidecar.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let sidecar = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        let request: serde_json::Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(request["kind"], "start");
        let reply = serde_json::json!({"version":1,"kind":"reply","requestId":request["requestId"],"ok":true});
        write.write_all(format!("{reply}\n").as_bytes()).await.unwrap();
        let _ = lines.next_line().await;
    });
    let bridge = augmentagent_approval_discord::voice_bridge::VoiceBridge::connect(&socket).await.unwrap();
    bridge.start(augmentagent_approval_discord::voice_bridge::VoiceBinding {
        guild_id:"1".into(), conversation_id:"1:2".into(), text_channel_id:"2".into(),
        voice_channel_id:"3".into(), owner_id:"4".into(), bot_user_id:"5".into(), generation:6,
        stt_provider: None, tts_provider: None,
    }).await.unwrap();
    let service = augmentagent_approval_discord::voice_tool::VoiceToolService::start(&bridge).await.unwrap();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let reasoner = Arc::new(FallbackReasoner::for_tests(
        vec![(ProviderKind::Claude, Arc::new(McpCaptureFixture(Arc::clone(&captured))))],
        augmentagent_channel_core::cooldown::CooldownLatch::at(root.path().join("cooldowns.json")),
    ));
    let wiki = root.path().join("wiki");
    std::fs::create_dir(&wiki).unwrap();
    let handler = WikiQuerier {
        reasoner, wiki_root: wiki, repo_root: root.path().to_path_buf(),
        conversation_store: None,
        conversation_scheduler: Arc::new(augmentagent_approval_discord::conversation::ConversationScheduler::new()),
        voice_enabled: true,
        voice_tools: std::sync::OnceLock::new(), final_spoken_turns: dashmap::DashMap::new(),
    };
    handler.attach_voice_tools(Arc::clone(&service));
    let mut ctx = augmentagent_approval_discord::AuditCtx {
        session_id:"2:10".into(), guild_id:Some(1), http:None,
        channel_id:Some(serenity::model::id::ChannelId::new(2)), owner_authorized:true,
    };
    handler.answer(&ctx, "hello").await.unwrap();
    ctx.channel_id = Some(serenity::model::id::ChannelId::new(9));
    handler.answer(&ctx, "other conversation").await.unwrap();
    ctx.channel_id = Some(serenity::model::id::ChannelId::new(2));
    ctx.owner_authorized = false;
    handler.answer(&ctx, "not owner").await.unwrap();
    let calls = captured.lock().unwrap();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0].0.as_ref().unwrap()["mcpServers"]["voice"]["args"],
        serde_json::json!(["voice-tool"]));
    assert!(calls[0].1.contains(&"mcp__voice__speak".to_string()));
    for (settings, tools) in &calls[1..] {
        assert!(settings.as_ref().unwrap()["mcpServers"].get("voice").is_none());
        assert!(!tools.iter().any(|tool| tool.starts_with("mcp__voice__")));
    }
    sidecar.abort();
}

struct SessionFixture {
    calls: Arc<Mutex<Vec<(String, String)>>>,
}

struct LegacyProfileFixture;

#[async_trait]
impl Reasoner for LegacyProfileFixture {
    async fn call(&self, _opts: &ReasonerOpts, _prompt: &str) -> anyhow::Result<String> {
        Ok("legacy profile reply".into())
    }
}

#[tokio::test]
async fn voice_flag_preserves_unbound_qwen_and_glm_text_routes() {
    let Ok(root) = std::env::var("VOICE_LEGACY_PROFILES_TEST_ROOT") else {
        let dir = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "discord_voice_session_tests::voice_flag_preserves_unbound_qwen_and_glm_text_routes", "--nocapture"])
            .env("VOICE_LEGACY_PROFILES_TEST_ROOT", dir.path())
            .env("AUGMENTAGENT_MODEL_SELECTION_CONFIG", dir.path().join("selection.json"))
            .env("AUGMENTAGENT_MODEL_ROUTER_CONFIG", dir.path().join("router.json"))
            .env("AUGMENTAGENT_MODEL_QWEN_ENABLED", "1")
            .env("AUGMENTAGENT_MODEL_GLM_ENABLED", "1")
            .output().unwrap();
        assert!(output.status.success(), "{}\n{}",
            String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        return;
    };
    let root = PathBuf::from(root);
    std::fs::write(root.join("router.json"), serde_json::json!({
        "version": 1, "mode": "direct", "base_url": "http://127.0.0.1:1/v1",
        "api_key": "synthetic-test-key",
        "models": {
            "claude": {"quality": "cc/synthetic", "fast": "cc/synthetic"},
            "codex": {"quality": "cx/synthetic", "fast": "cx/synthetic"}
        }
    }).to_string()).unwrap();
    let wiki = root.join("wiki");
    std::fs::create_dir_all(&wiki).unwrap();
    let store = Arc::new(Store::open(root.join("data.db")).unwrap());
    let selection = augmentagent_channel_core::model_selection::SelectionStore::new(
        root.join("selection.json"));
    selection.set(Some("2"), Some(ProviderKind::Qwen)).unwrap();
    selection.set(Some("3"), Some(ProviderKind::Glm)).unwrap();
    let fixture = Arc::new(LegacyProfileFixture);
    let reasoner = Arc::new(FallbackReasoner::for_tests(vec![
        (ProviderKind::Qwen, fixture.clone() as Arc<dyn Reasoner>),
        (ProviderKind::Glm, fixture as Arc<dyn Reasoner>),
    ], augmentagent_channel_core::cooldown::CooldownLatch::at(root.join("cooldowns.json"))));
    let handler = WikiQuerier {
        reasoner, wiki_root: wiki, repo_root: root,
        conversation_store: Some(Arc::clone(&store)),
        conversation_scheduler: Arc::new(augmentagent_approval_discord::conversation::ConversationScheduler::new()),
        voice_enabled: true, voice_tools: std::sync::OnceLock::new(),
        final_spoken_turns: dashmap::DashMap::new(),
    };
    for channel in [2, 3] {
        let ctx = augmentagent_approval_discord::AuditCtx {
            session_id: format!("{channel}:1"), guild_id: Some(1), http: None,
            channel_id: Some(serenity::model::id::ChannelId::new(channel)), owner_authorized: true,
        };
        assert_eq!(handler.answer_turn(&ctx, "older text", "new text").await.unwrap(), "legacy profile reply");
        assert!(store.discord_conversation("1", &channel.to_string()).unwrap().is_none());
    }
}

#[async_trait]
impl Reasoner for SessionFixture {
    async fn call(&self, _opts: &ReasonerOpts, prompt: &str) -> anyhow::Result<String> {
        let session = CURRENT.try_with(Arc::clone)?;
        let mut lease = session.begin(ProviderKind::Claude)?;
        let id = match lease.launch() {
            Launch::Create { requested_id: Some(id) } | Launch::Resume { id } => id,
            _ => anyhow::bail!("expected Claude session"),
        };
        lease.observe(&id)?;
        lease.finish()?;
        self.calls.lock().unwrap().push((id, prompt.to_string()));
        Ok("SYNTHETIC_VOICE_SESSION_REPLY".into())
    }
}

#[tokio::test]
async fn guild_text_uses_one_native_session_and_bootstraps_history_only_once() {
    let Ok(root) = std::env::var("VOICE_SESSION_TEST_ROOT") else {
        let dir = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "discord_voice_session_tests::guild_text_uses_one_native_session_and_bootstraps_history_only_once", "--nocapture"])
            .env("VOICE_SESSION_TEST_ROOT", dir.path())
            .env("AUGMENTAGENT_MODEL_SELECTION_CONFIG", dir.path().join("selection.json"))
            .output().unwrap();
        assert!(output.status.success(), "{}\n{}",
            String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        return;
    };
    let root = PathBuf::from(root);
    let wiki = root.join("wiki");
    std::fs::create_dir_all(&wiki).unwrap();
    let store = Arc::new(Store::open(root.join("data.db")).unwrap());
    let calls = Arc::new(Mutex::new(Vec::new()));
    let fixture = Arc::new(SessionFixture { calls: Arc::clone(&calls) });
    let reasoner = Arc::new(FallbackReasoner::for_tests(
        vec![(ProviderKind::Claude, fixture)],
        augmentagent_channel_core::cooldown::CooldownLatch::at(root.join("cooldowns.json")),
    ));
    let handler = WikiQuerier {
        reasoner,
        wiki_root: wiki,
        repo_root: root.clone(),
        conversation_store: Some(Arc::clone(&store)),
        conversation_scheduler: Arc::new(augmentagent_approval_discord::conversation::ConversationScheduler::new()),
        voice_enabled: true,
        voice_tools: std::sync::OnceLock::new(),
        final_spoken_turns: dashmap::DashMap::new(),
    };
    let mut ctx = augmentagent_approval_discord::AuditCtx {
        session_id: "2:1".into(), guild_id: Some(1), http: None,
        channel_id: Some(serenity::model::id::ChannelId::new(2)), owner_authorized: true,
    };
    handler.answer_turn(&ctx, "user: earlier", "first turn").await.unwrap();
    let refused = handler.model_command_in_guild(Some(1), 2, "/model set codex").await.unwrap();
    assert!(refused.contains("bound to native claude session"), "{refused}");
    ctx.session_id = "2:2".into();
    handler.answer_turn(&ctx, "user: earlier\nassistant: first reply", "second turn").await.unwrap();
    handler.answer_turn(&ctx, "different history", "duplicate second turn").await.unwrap();
    let other_ctx = augmentagent_approval_discord::AuditCtx {
        session_id: "3:1".into(), guild_id: Some(1), http: None,
        channel_id: Some(serenity::model::id::ChannelId::new(3)), owner_authorized: true,
    };
    handler.answer_turn(&other_ctx, "", "other thread").await.unwrap();
    let rows = calls.lock().unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].0, rows[1].0);
    assert_ne!(rows[0].0, rows[2].0);
    assert!(rows[0].1.contains("user: earlier"));
    assert!(!rows[1].1.contains("user: earlier"));
    let binding = store.discord_conversation("1", "2").unwrap().unwrap();
    assert_eq!(binding.native_session_id, rows[0].0);
    assert_eq!(binding.provider, "claude");
    assert!(!binding.uncertain);
}

struct UncertainFixture;

#[async_trait]
impl Reasoner for UncertainFixture {
    async fn call(&self, _opts: &ReasonerOpts, _prompt: &str) -> anyhow::Result<String> {
        let session = CURRENT.try_with(Arc::clone)?;
        let mut lease = session.begin(ProviderKind::Claude)?;
        let Launch::Create { requested_id: Some(id) } = lease.launch() else {
            anyhow::bail!("expected new Claude session");
        };
        lease.observe(&id)?;
        anyhow::bail!("synthetic CLI failure after session identity was observed")
    }
}

#[tokio::test]
async fn uncertain_native_turn_is_persisted_and_blocks_replay() {
    let Ok(root) = std::env::var("VOICE_UNCERTAIN_TEST_ROOT") else {
        let dir = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "discord_voice_session_tests::uncertain_native_turn_is_persisted_and_blocks_replay", "--nocapture"])
            .env("VOICE_UNCERTAIN_TEST_ROOT", dir.path())
            .env("AUGMENTAGENT_MODEL_SELECTION_CONFIG", dir.path().join("selection.json"))
            .output().unwrap();
        assert!(output.status.success(), "{}\n{}",
            String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        return;
    };
    let root = PathBuf::from(root);
    let wiki = root.join("wiki");
    std::fs::create_dir_all(&wiki).unwrap();
    let store = Arc::new(Store::open(root.join("data.db")).unwrap());
    let reasoner = Arc::new(FallbackReasoner::for_tests(
        vec![(ProviderKind::Claude, Arc::new(UncertainFixture))],
        augmentagent_channel_core::cooldown::CooldownLatch::at(root.join("cooldowns.json")),
    ));
    let handler = WikiQuerier {
        reasoner, wiki_root: wiki, repo_root: root.clone(),
        conversation_store: Some(Arc::clone(&store)),
        conversation_scheduler: Arc::new(augmentagent_approval_discord::conversation::ConversationScheduler::new()),
        voice_enabled: true,
        voice_tools: std::sync::OnceLock::new(),
        final_spoken_turns: dashmap::DashMap::new(),
    };
    let mut ctx = augmentagent_approval_discord::AuditCtx {
        session_id: "2:1".into(), guild_id: Some(1), http: None,
        channel_id: Some(serenity::model::id::ChannelId::new(2)), owner_authorized: true,
    };
    assert!(handler.answer_turn(&ctx, "", "first turn").await.is_err());
    drop(store);
    let reopened = Store::open(root.join("data.db")).unwrap();
    let persisted = reopened.discord_conversation("1", "2").unwrap().unwrap();
    assert!(persisted.uncertain);
    ctx.session_id = "2:2".into();
    let second = handler.answer_turn(&ctx, "", "do not replay").await.unwrap_err();
    assert!(second.to_string().contains("uncertain turn"));
}

struct NoIdFixture(Arc<std::sync::atomic::AtomicUsize>);

#[async_trait]
impl Reasoner for NoIdFixture {
    async fn call(&self, _opts: &ReasonerOpts, _prompt: &str) -> anyhow::Result<String> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        anyhow::bail!("synthetic transport failure before session identity was observed")
    }
}

#[tokio::test]
async fn failed_turn_without_native_id_is_not_replayed_after_restart() {
    let Ok(root) = std::env::var("VOICE_NO_ID_TEST_ROOT") else {
        let dir = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "discord_voice_session_tests::failed_turn_without_native_id_is_not_replayed_after_restart", "--nocapture"])
            .env("VOICE_NO_ID_TEST_ROOT", dir.path())
            .env("AUGMENTAGENT_MODEL_SELECTION_CONFIG", dir.path().join("selection.json"))
            .output().unwrap();
        assert!(output.status.success(), "{}\n{}",
            String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        return;
    };
    let root = PathBuf::from(root);
    let wiki = root.join("wiki");
    std::fs::create_dir_all(&wiki).unwrap();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let reasoner = Arc::new(FallbackReasoner::for_tests(
        vec![(ProviderKind::Claude, Arc::new(NoIdFixture(Arc::clone(&calls))))],
        augmentagent_channel_core::cooldown::CooldownLatch::at(root.join("cooldowns.json")),
    ));
    let ctx = augmentagent_approval_discord::AuditCtx {
        session_id: "2:1".into(), guild_id: Some(1), http: None,
        channel_id: Some(serenity::model::id::ChannelId::new(2)), owner_authorized: true,
    };
    for _ in 0..2 {
        let handler = WikiQuerier {
            reasoner: Arc::clone(&reasoner), wiki_root: wiki.clone(), repo_root: root.clone(),
            conversation_store: Some(Arc::new(Store::open(root.join("data.db")).unwrap())),
            conversation_scheduler: Arc::new(augmentagent_approval_discord::conversation::ConversationScheduler::new()),
            voice_enabled: true,
            voice_tools: std::sync::OnceLock::new(),
            final_spoken_turns: dashmap::DashMap::new(),
        };
        assert!(handler.answer_turn(&ctx, "", "must not replay").await.is_err());
    }
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    let handler = WikiQuerier {
        reasoner, wiki_root: wiki, repo_root: root.clone(),
        conversation_store: Some(Arc::new(Store::open(root.join("data.db")).unwrap())),
        conversation_scheduler: Arc::new(augmentagent_approval_discord::conversation::ConversationScheduler::new()),
        voice_enabled: true, voice_tools: std::sync::OnceLock::new(),
        final_spoken_turns: dashmap::DashMap::new(),
    };
    let next = augmentagent_approval_discord::AuditCtx { session_id: "2:2".into(), ..ctx };
    assert!(handler.answer_turn(&next, "", "new turn after pre-launch failure").await.is_err());
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
}
