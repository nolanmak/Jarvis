//! #1288 — the shared session conformance scenario
//! (`augmentagent_channel_core::surface_conformance`) against both owner
//! conversation surfaces, each running the production agent (`WikiQuerier`
//! over `FallbackReasoner`) with only the provider replaced by the offline
//! recording stand-in:
//!
//! * Discord: `WikiQuerier::answer_turn` for a guild text channel (the
//!   native-session route: voice enabled, explicit owner).
//! * Slack: `SlackConversationHarness` (what `serve` runs) over the same
//!   `WikiQuerier`, with the turns the Slack dispatcher builds for a
//!   control-channel thread.
//!
//! Each test re-runs itself in a child process so the model-selection file
//! and the state directory are private to it.

use super::*;
use augmentagent_approval_discord::conversation::ConversationScheduler;
use augmentagent_channel_core::providers::ProviderKind;
use augmentagent_channel_core::surface_conformance::{
    native_session_conformance, ConformanceAdapter, RecordingNativeProvider,
};
use augmentagent_channel_core::Reasoner;
use augmentagent_channel_slack::harness::SlackConversationHarness;
use augmentagent_channel_slack::interactive::{slack_turn_id, SlackTurn, SlackTurnHandler};
use augmentagent_channel_slack::owner::OwnerInputSource;
use augmentagent_channel_slack::surface::SlackWorkspace;
use augmentagent_channel_slack::transport::event::{parse_envelope, Envelope};
use std::sync::Mutex;

/// Re-run `test` in a child with a private selection file and state dir.
/// Returns the fixture root when already inside the child.
fn isolated(test: &str, marker: &str) -> Option<PathBuf> {
    if let Ok(root) = std::env::var(marker) {
        return Some(PathBuf::from(root));
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("state dir \u{fc}");
    std::fs::create_dir_all(&root).unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env(marker, &root)
        .env(
            "AUGMENTAGENT_MODEL_SELECTION_CONFIG",
            root.join("selection.json"),
        )
        .env("XDG_STATE_HOME", root.join("xdg"))
        .env("AUGMENTAGENT_TOOL_AUDIT_LOG", root.join("audit.jsonl"))
        .env_remove("AUGMENTAGENT_DB")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    None
}

fn querier(root: &Path, provider: &Arc<RecordingNativeProvider>) -> WikiQuerier {
    let wiki = root.join("wiki");
    std::fs::create_dir_all(&wiki).unwrap();
    WikiQuerier {
        reasoner: Arc::new(FallbackReasoner::for_tests(
            vec![(
                ProviderKind::Claude,
                Arc::clone(provider) as Arc<dyn Reasoner>,
            )],
            augmentagent_channel_core::cooldown::CooldownLatch::at(root.join("cooldowns.json")),
        )),
        wiki_root: wiki,
        repo_root: root.to_path_buf(),
        conversation_store: Some(Arc::new(Store::open(root.join("data.db")).unwrap())),
        conversation_scheduler: Arc::new(ConversationScheduler::new()),
        voice_enabled: true,
        voice_tools: std::sync::OnceLock::new(),
        final_spoken_turns: dashmap::DashMap::new(),
    }
}

struct DiscordAdapter {
    root: PathBuf,
    provider: Arc<RecordingNativeProvider>,
    querier: Mutex<Arc<WikiQuerier>>,
}

#[async_trait]
impl ConformanceAdapter for DiscordAdapter {
    fn name(&self) -> &str {
        "discord"
    }

    async fn turn(&self, conversation: &str, turn: &str, text: &str) -> anyhow::Result<String> {
        let channel: u64 = if conversation == "A" { 2 } else { 3 };
        let ctx = augmentagent_approval_discord::AuditCtx {
            session_id: format!("{channel}:{turn}"),
            guild_id: Some(1),
            http: None,
            channel_id: Some(serenity::model::id::ChannelId::new(channel)),
            owner_authorized: true,
        };
        let querier = self.querier.lock().unwrap().clone();
        querier.answer_turn(&ctx, "", text).await
    }

    async fn restart(&self) {
        *self.querier.lock().unwrap() = Arc::new(querier(&self.root, &self.provider));
    }
}

const TEAM: &str = "T00000001";
const CONTROL: &str = "C00000001";

struct SlackAdapter {
    root: PathBuf,
    provider: Arc<RecordingNativeProvider>,
    harness: Mutex<Arc<SlackConversationHarness>>,
}

impl SlackAdapter {
    fn harness(
        root: &Path,
        provider: &Arc<RecordingNativeProvider>,
    ) -> Arc<SlackConversationHarness> {
        let query = Arc::new(querier(root, provider));
        let store = Arc::new(Store::open(root.join("data.db")).unwrap());
        Arc::new(SlackConversationHarness::new(
            store,
            query,
            root.join("wiki"),
        ))
    }
}

#[async_trait]
impl ConformanceAdapter for SlackAdapter {
    fn name(&self) -> &str {
        "slack"
    }

    async fn turn(&self, conversation: &str, turn: &str, text: &str) -> anyhow::Result<String> {
        let parent = if conversation == "A" {
            "1700000800.000000"
        } else {
            "1700000900.000000"
        };
        let ts = format!("{}.00000{turn}", &parent[..10]);
        let frame = serde_json::json!({
            "type": "events_api", "envelope_id": "c",
            "payload": {"team_id": TEAM, "event_id": "Evc",
                "event": {"type": "message", "channel": CONTROL, "channel_type": "group",
                    "user": "U00000001", "text": text, "ts": ts, "thread_ts": parent}}
        });
        let Ok(Envelope::Event(envelope)) = parse_envelope(&frame.to_string()) else {
            anyhow::bail!("parse")
        };
        let ws = SlackWorkspace::new(TEAM, None)?;
        let session = ws.conversation(CONTROL, Some(parent))?;
        let event_id = format!("{CONTROL}:{ts}");
        let turn = SlackTurn {
            turn_id: slack_turn_id(&ws.account(), &event_id),
            event_id,
            attempt: 1,
            owner: ws.owner("U00000001")?,
            conversation: Some(session.clone()),
            session: Some(session),
            source: OwnerInputSource::ThreadReply,
            text: text.into(),
            prompt: text.into(),
            inbound_dir: None,
            cancel: tokio_util::sync::CancellationToken::new(),
            envelope: *envelope,
        };
        let harness = self.harness.lock().unwrap().clone();
        Ok(harness
            .handle_turn(&turn)
            .await?
            .map(|r| r.text)
            .unwrap_or_default())
    }

    async fn restart(&self) {
        *self.harness.lock().unwrap() = Self::harness(&self.root, &self.provider);
    }
}

#[tokio::test]
async fn discord_conversation_turns_pass_the_shared_session_conformance_scenario() {
    let Some(root) = isolated(
        "surface_conformance_tests::discord_conversation_turns_pass_the_shared_session_conformance_scenario",
        "SURFACE_CONFORMANCE_DISCORD_ROOT",
    ) else {
        return;
    };
    let provider = Arc::new(RecordingNativeProvider::new(ProviderKind::Claude));
    let adapter = DiscordAdapter {
        querier: Mutex::new(Arc::new(querier(&root, &provider))),
        root,
        provider: Arc::clone(&provider),
    };
    native_session_conformance(&adapter, &provider).await;
}

#[tokio::test]
async fn slack_conversation_turns_pass_the_shared_session_conformance_scenario() {
    let Some(root) = isolated(
        "surface_conformance_tests::slack_conversation_turns_pass_the_shared_session_conformance_scenario",
        "SURFACE_CONFORMANCE_SLACK_ROOT",
    ) else {
        return;
    };
    let provider = Arc::new(RecordingNativeProvider::new(ProviderKind::Claude));
    let adapter = SlackAdapter {
        harness: Mutex::new(SlackAdapter::harness(&root, &provider)),
        root: root.clone(),
        provider: Arc::clone(&provider),
    };
    native_session_conformance(&adapter, &provider).await;
    // The same production agent ran each Slack turn: every provider call was
    // stamped with the turn's audit ID and the owner rules / clock preamble
    // WikiQuerier adds, exactly as for Discord.
    for call in provider.calls() {
        let id = call.audit_session_id.unwrap_or_default();
        assert!(id.starts_with("slack:T00000001:C00000001:"), "{id}");
    }
}
