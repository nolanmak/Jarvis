use super::*;
use augmentagent_channel_core::{
    native_session::{Launch, CURRENT},
    providers::ProviderKind,
    Reasoner, ReasonerOpts,
};
use std::sync::Mutex;

struct SessionFixture {
    calls: Arc<Mutex<Vec<(String, String)>>>,
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
        };
        assert!(handler.answer_turn(&ctx, "", "must not replay").await.is_err());
    }
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}
