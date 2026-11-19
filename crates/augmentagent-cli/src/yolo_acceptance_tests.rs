//! #2220 — acceptance for yolo isolation (spec #2243 section 5.6).
//!
//! One canary string is used in a yolo thread three ways: in the owner's
//! prompt, in a workspace file the model writes, and in the model's reply (as
//! a fetched page would arrive). Normal turns then run in the parent channel
//! and in a DM. The canary must reach neither prompt, no message search, and
//! no owner chat history.
//!
//! The handler-level steps (the gate in the message handler, the history
//! fetch per entry point, the mirror check) have their own tests beside the
//! code in the Discord crate; the vendor launch is tested in the core crate.
//! This test joins the layers below the handler in one sequence.
use augmentagent_channel_core::providers::ProviderKind;
use augmentagent_channel_core::{yolo_preset, Reasoner, ReasonerOpts};
use augmentagent_store::yolo_threads::{override_path_for_this_thread, Registry};
use std::sync::{Arc, Mutex};

const CANARY: &str = "canary-7f3a91c2-yolo-only";
const YOLO_THREAD: u64 = 1001;
const PARENT: u64 = 77;
const DM: u64 = 42;

#[derive(Default)]
struct Calls(Mutex<Vec<(ReasonerOpts, String)>>);

impl Calls {
    fn take(&self) -> Vec<(ReasonerOpts, String)> {
        std::mem::take(&mut self.0.lock().unwrap())
    }
}

/// Stands in for the unfiltered model: saves a file in its workspace and
/// answers with what it "found".
struct YoloModel(Arc<Calls>);

#[async_trait::async_trait]
impl Reasoner for YoloModel {
    async fn call(&self, opts: &ReasonerOpts, prompt: &str) -> anyhow::Result<String> {
        let workspace = opts.cwd.clone().expect("a yolo turn runs in its workspace");
        std::fs::write(workspace.join("notes.txt"), CANARY)?;
        self.0 .0.lock().unwrap().push((opts.clone(), prompt.to_string()));
        Ok(format!("The page says {CANARY}."))
    }
}

struct NormalModel(Arc<Calls>);

#[async_trait::async_trait]
impl Reasoner for NormalModel {
    async fn call(&self, opts: &ReasonerOpts, prompt: &str) -> anyhow::Result<String> {
        self.0 .0.lock().unwrap().push((opts.clone(), prompt.to_string()));
        Ok("normal reply".into())
    }
}

fn ctx(session: &str, channel: u64, guild: Option<u64>) -> augmentagent_approval_discord::AuditCtx {
    augmentagent_approval_discord::AuditCtx {
        session_id: session.into(),
        guild_id: guild,
        http: None,
        channel_id: Some(serenity::model::id::ChannelId::new(channel)),
        owner_authorized: true,
    }
}

fn discord_row(message_id: &str, channel: u64, body: &str) -> augmentagent_store::Email {
    augmentagent_store::Email {
        message_id: message_id.into(),
        thread_id: Some(channel.to_string()),
        from: "owner".into(),
        to: String::new(),
        cc: String::new(),
        attachments: Vec::new(),
        subject: "Discord message".into(),
        body: body.into(),
        date: "2026-11-19T12:00:00Z".into(),
        account_entity_id: None,
        platform: "discord".into(),
        kind: "guild_channel".into(),
    }
}

fn discord_message(channel: u64, text: &str) -> serenity::all::Message {
    let mut message = serenity::all::Message::default();
    message.channel_id = serenity::model::id::ChannelId::new(channel);
    message.guild_id = Some(serenity::model::id::GuildId::new(5));
    message.content = text.into();
    message
}

#[tokio::test]
async fn nothing_from_a_yolo_thread_reaches_a_normal_turn() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let wiki = root.path().join("wiki");
    std::fs::create_dir_all(&wiki).unwrap();
    let registry = Registry::new(root.path().join("yolo-threads.json"));
    registry.record(YOLO_THREAD, PARENT, 1).unwrap();
    override_path_for_this_thread(Some(registry.path().to_path_buf()));

    let db = root.path().join("db");
    let store = Arc::new(crate::Store::open(&db).unwrap());
    let yolo_calls = Arc::new(Calls::default());
    let normal_calls = Arc::new(Calls::default());
    let reasoner = Arc::new(crate::FallbackReasoner::for_tests(
        vec![
            (ProviderKind::Claude, Arc::new(NormalModel(normal_calls.clone())) as Arc<dyn Reasoner>),
            (ProviderKind::Yolo, Arc::new(YoloModel(yolo_calls.clone())) as Arc<dyn Reasoner>),
        ],
        augmentagent_channel_core::cooldown::CooldownLatch::at(root.path().join("cooldowns.json")),
    ));
    let handler = crate::wiki_querier(
        reasoner,
        wiki.clone(),
        root.path().to_path_buf(),
        store.clone(),
        false,
    );
    use augmentagent_approval_discord::QueryHandler as _;

    // 1. A yolo turn in the thread. Only the yolo model answers it.
    let yolo_ctx = ctx("1001:1", YOLO_THREAD, Some(5));
    let reply = augmentagent_approval_discord::interaction::yolo_turn(
        true,
        crate::yolo_turn::STATE_DIR_FOR_TESTS.scope(
            state.clone(),
            handler.answer(&yolo_ctx, &format!("look up {CANARY} and save it")),
        ),
    )
    .await
    .unwrap();
    assert!(reply.contains(CANARY), "{reply}");
    assert!(normal_calls.take().is_empty(), "no other model answers a yolo turn");
    let yolo = yolo_calls.take();
    assert_eq!(yolo.len(), 1);
    let (yolo_opts, yolo_prompt) = &yolo[0];
    assert!(yolo_prompt.contains(CANARY));

    // The turn ran in its own workspace under the state directory, and the
    // file it wrote is there.
    let workspace = state.join("yolo-workspaces").join(YOLO_THREAD.to_string());
    assert_eq!(yolo_opts.cwd.as_deref(), Some(workspace.as_path()));
    assert_eq!(std::fs::read_to_string(workspace.join("notes.txt")).unwrap(), CANARY);
    // It had no tool that writes somewhere a normal turn reads: no memory or
    // wiki write, no provider switch, nothing outbound.
    for tool in &yolo_opts.allowed_tools {
        if tool.starts_with("mcp__memory__") {
            assert!(yolo_preset::MEMORY_TOOLS.contains(&tool.as_str()), "{tool}");
        }
        assert!(!tool.contains("set_provider"), "{tool}");
        assert!(!tool.contains("memory_write") && !tool.contains("memory_store"), "{tool}");
        for outbound in ["send", "draft", "propose", "create_event", "compose"] {
            assert!(!tool.to_lowercase().contains(outbound), "{tool}");
        }
    }

    // 2. Both stored-message writers refuse the thread's messages, so the
    //    message search that the memory tools use cannot return them.
    assert!(!store
        .upsert_email(&discord_row("9001", YOLO_THREAD, &format!("look up {CANARY}")))
        .unwrap());
    assert!(!store
        .upsert_email_backfill(&discord_row("9002", YOLO_THREAD, &reply), 1)
        .unwrap());
    assert!(store
        .upsert_email(&discord_row("9003", PARENT, "an ordinary question"))
        .unwrap());
    // Index what was stored, as the daemon's indexer does.
    augmentagent_messages::index::drain(&store, 100, std::time::Duration::ZERO).unwrap();
    let conn = rusqlite::Connection::open(&db).unwrap();
    let found = augmentagent_messages::query::search(&conn, CANARY, Some(50), 0).unwrap();
    assert!(found.hits.is_empty(), "{}", serde_json::to_string(&found).unwrap());
    // Control: the same search does find the parent channel's row.
    let control = augmentagent_messages::query::search(&conn, "ordinary question", Some(50), 0).unwrap();
    assert_eq!(control.hits.len(), 1, "{}", serde_json::to_string(&control).unwrap());
    let stored: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM emails WHERE body LIKE ?1 OR subject LIKE ?1",
            [format!("%{CANARY}%")],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored, 0);

    // 3. The owner mirror takes nothing from the thread, so the owner chat
    //    history has nothing to show.
    let surface = augmentagent_approval_discord::imessage_owner::OwnerSurface::new(store.clone());
    surface
        .mirror_discord(&discord_message(YOLO_THREAD, &reply), Some(PARENT), None)
        .unwrap();
    let history = store.imessage_owner_history(None, 0, 50).unwrap().to_string();
    assert!(!history.contains(CANARY), "{history}");

    // 4. Normal turns in the parent channel and in a DM.
    for (session, channel, guild) in [("77:1", PARENT, Some(5)), ("42:1", DM, None)] {
        let answer = handler
            .answer(&ctx(session, channel, guild), "what did we talk about earlier?")
            .await
            .unwrap();
        assert_eq!(answer, "normal reply");
    }
    assert!(yolo_calls.take().is_empty(), "the yolo model never answers a normal turn");
    let normal = normal_calls.take();
    assert_eq!(normal.len(), 2);
    for (opts, prompt) in &normal {
        assert!(!prompt.contains(CANARY), "{prompt}");
        assert!(!opts.system_prompt.contains(CANARY));
        // The yolo workspace is outside everything a normal turn can read.
        let mut roots = opts.add_dirs.clone();
        roots.extend(opts.cwd.clone());
        for dir in &roots {
            assert!(!workspace.starts_with(dir), "{} exposes the workspace", dir.display());
            assert!(!dir.starts_with(&state), "{} is inside the state directory", dir.display());
        }
    }

    // 5. A turn that is not a yolo turn cannot run in the thread at all.
    let refused = handler
        .answer(&ctx("1001:2", YOLO_THREAD, Some(5)), "summarise this thread")
        .await
        .unwrap();
    assert_eq!(refused, "Not available in a yolo thread.");
    assert!(normal_calls.take().is_empty());
    override_path_for_this_thread(None);
}
