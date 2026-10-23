//! #1292 — Slack owner commands, below the socket: parsing, generated help,
//! execution, persistence and replies for every command in the shared
//! registry, and loop delivery to a Slack destination.
//!
//! Temporary stores and selection files, fake process walker and signaler,
//! fake journal, deterministic loop parser, and a paused tokio clock for the
//! loop scheduler. Identifiers are synthetic.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use augmentagent_approval_discord::owner_commands::{SlackMapping, OWNER_COMMANDS};
use augmentagent_approval_discord::{
    JournalOps, LoopCommandParser, LoopPoster, LoopRunner, LoopScheduler, ParsedLoop,
    JOURNAL_NOT_CONFIGURED,
};
use augmentagent_channel_core::providers::ProviderKind;
use augmentagent_channel_slack::commands::{
    help_text, parse_slack_loop_ref, recognize, slack_loop_owner, slack_loop_ref, slack_selection,
    CommandContext, ConversationControl, ProcessControl, Recognized, SlackCommandDeps,
    SlackCommands, SlackLoopPoster, SurfaceGatedRunner, SurfaceLoopPoster,
};
use augmentagent_channel_slack::surface::SlackWorkspace;
use augmentagent_loops::{ClaudeProc, ProcSource, Signaler};
use augmentagent_store::delivery::NewInboundEvent;
use augmentagent_store::{
    NativeConversation, Store, SurfaceConversationRef, SurfaceOwnerRef, SurfaceTurnRef,
};

const TEAM: &str = "T00000001";
const OWNER: &str = "U00000001";
const DM: &str = "D00000001";
const CONTROL: &str = "C00000001";
const THREAD: &str = "1700000100.000100";

fn workspace() -> SlackWorkspace {
    SlackWorkspace::new(TEAM, None).unwrap()
}

fn owner() -> SurfaceOwnerRef {
    workspace().owner(OWNER).unwrap()
}

fn dm() -> SurfaceConversationRef {
    workspace().conversation(DM, None).unwrap()
}

fn thread() -> SurfaceConversationRef {
    workspace().conversation(CONTROL, Some(THREAD)).unwrap()
}

fn channel() -> SurfaceConversationRef {
    workspace().conversation(CONTROL, None).unwrap()
}

// ---------------------------------------------------------------------------
// Fakes
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FakeControl {
    running: Mutex<Vec<String>>,
    cancelled: Mutex<Vec<String>>,
}

impl FakeControl {
    fn running(conversation: &SurfaceConversationRef) -> Self {
        let c = Self::default();
        c.running.lock().unwrap().push(conversation.storage_key());
        c
    }
}

impl ConversationControl for FakeControl {
    fn is_running(&self, conversation: &SurfaceConversationRef) -> bool {
        self.running
            .lock()
            .unwrap()
            .contains(&conversation.storage_key())
    }
    fn cancel_running(&self, conversation: &SurfaceConversationRef) -> bool {
        let key = conversation.storage_key();
        let mut running = self.running.lock().unwrap();
        let before = running.len();
        running.retain(|k| *k != key);
        if running.len() != before {
            self.cancelled.lock().unwrap().push(key);
            true
        } else {
            false
        }
    }
}

struct FakeProcs {
    procs: Vec<ClaudeProc>,
    self_pid: i32,
    parents: HashMap<i32, i32>,
    fail: bool,
}

impl ProcSource for FakeProcs {
    fn list(&self) -> anyhow::Result<Vec<ClaudeProc>> {
        if self.fail {
            anyhow::bail!("ps: command not found");
        }
        Ok(self.procs.clone())
    }
    fn self_pid(&self) -> i32 {
        self.self_pid
    }
    fn parent_of(&self, pid: i32) -> Option<i32> {
        self.parents.get(&pid).copied()
    }
}

#[derive(Default)]
struct FakeSignals {
    sent: Mutex<Vec<(i32, &'static str)>>,
}

impl Signaler for FakeSignals {
    fn term(&self, pid: i32) -> std::io::Result<()> {
        self.sent.lock().unwrap().push((pid, "TERM"));
        Ok(())
    }
    fn kill(&self, pid: i32) -> std::io::Result<()> {
        self.sent.lock().unwrap().push((pid, "KILL"));
        Ok(())
    }
    fn alive(&self, _pid: i32) -> bool {
        false
    }
}

fn proc(pid: i32, ppid: i32) -> ClaudeProc {
    ClaudeProc {
        pid,
        ppid,
        elapsed_secs: 75,
        cwd: Some(PathBuf::from("/work")),
        cmdline: "claude --print".into(),
    }
}

#[derive(Default)]
struct FakeJournal {
    saved: Mutex<Vec<String>>,
}

#[async_trait]
impl JournalOps for FakeJournal {
    async fn save_text(&self, _title: Option<String>, text: &str) -> Result<String, String> {
        self.saved.lock().unwrap().push(text.to_string());
        Ok(format!("Saved to your journal: {text}"))
    }
    async fn compose_and_save(
        &self,
        _history: &str,
        _title: Option<String>,
    ) -> Result<String, String> {
        panic!("compose needs conversation history Slack does not provide yet")
    }
}

/// Stands in for the model-backed loop parser; counts its calls.
#[derive(Default)]
struct FakeParser {
    calls: AtomicUsize,
}

#[async_trait]
impl LoopCommandParser for FakeParser {
    async fn parse(&self, raw: &str, _model: Option<&str>) -> Result<ParsedLoop, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if raw.contains("each morning") {
            Ok(ParsedLoop {
                interval_secs: 86_400,
                prompt: "summarise my inbox".into(),
                duration_secs: None,
                cron_expr: None,
                tz: None,
                nag_until_ack: true,
            })
        } else {
            Err("couldn't understand that schedule".into())
        }
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    store: Arc<Store>,
    selection: PathBuf,
    signals: Arc<FakeSignals>,
    journal: Arc<FakeJournal>,
    parser: Arc<FakeParser>,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path().join("data.db")).unwrap());
        let selection = dir.path().join("config").join("model-selection.json");
        Fixture {
            _dir: dir,
            store,
            selection,
            signals: Arc::new(FakeSignals::default()),
            journal: Arc::new(FakeJournal::default()),
            parser: Arc::new(FakeParser::default()),
        }
    }

    fn deps(&self, procs: FakeProcs) -> SlackCommandDeps {
        let mut deps = SlackCommandDeps::new(self.selection.clone());
        deps.model_ready = Arc::new(|kind| match kind {
            ProviderKind::Glm => Err("glm is paused on this daemon".into()),
            _ => Ok(()),
        });
        deps.loop_parser = Some(self.parser.clone() as Arc<dyn LoopCommandParser>);
        deps.journal = Some(self.journal.clone() as Arc<dyn JournalOps>);
        deps.processes = ProcessControl {
            source: Arc::new(procs),
            signaler: self.signals.clone(),
            grace: Duration::ZERO,
        };
        deps
    }

    fn commands(&self) -> SlackCommands {
        self.commands_with(FakeProcs {
            procs: vec![proc(100, 1), proc(200, 1)],
            self_pid: 999,
            parents: HashMap::from([(999, 200), (200, 1)]),
            fail: false,
        })
    }

    fn commands_with(&self, procs: FakeProcs) -> SlackCommands {
        SlackCommands::new(Arc::clone(&self.store), self.deps(procs))
    }
}

async fn run(
    commands: &SlackCommands,
    conversation: &SurfaceConversationRef,
    control: &dyn ConversationControl,
    text: &str,
) -> String {
    let recognized = recognize(text, true).unwrap_or_else(|| panic!("{text:?} is a command"));
    commands
        .execute(
            &recognized,
            &CommandContext {
                owner: &owner(),
                conversation,
                control,
                now_ms: 1_700_000_000_000,
            },
        )
        .await
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

fn command(text: &str, slash: bool) -> Option<(&'static str, String)> {
    match recognize(text, slash)? {
        Recognized::Command { name, args } => Some((name, args)),
        Recognized::Unknown { .. } => Some(("<unknown>", String::new())),
    }
}

#[test]
fn slash_commands_take_any_registered_word_and_leave_other_text_to_the_agent() {
    assert_eq!(command("", true), Some(("help", String::new())));
    assert_eq!(command("help", true), Some(("help", String::new())));
    assert_eq!(command("help model", true), Some(("help", "model".into())));
    assert_eq!(
        command("model set codex", true),
        Some(("model", "set codex".into()))
    );
    assert_eq!(command("ps", true), Some(("processes", String::new())));
    assert_eq!(
        command("journal hello there", true),
        Some(("journal", "hello there".into()))
    );
    assert_eq!(command("NEW", true), Some(("reset", String::new())));
    assert_eq!(command("voice", true), Some(("voice", String::new())));
    assert_eq!(command("cancel all", true), Some(("cancel", "all".into())));
    // `/jarvis <question>` still talks to Jarvis (the manifest's promise).
    assert_eq!(command("what is due today?", true), None);
    // Approvals keep their own text commands (#1289).
    assert_eq!(command("approve 1234abcd", true), None);
    assert_eq!(command("approvals", true), None);
    // Bare `cancel`/`stop` keep the existing private-lane cancel path.
    assert_eq!(command("cancel", true), None);
    assert_eq!(command("stop", true), None);
}

#[test]
fn plain_text_needs_the_exact_word_a_discord_prefix_or_a_sigil() {
    // Exact: alone, or with `!`.
    assert_eq!(command("help", false), Some(("help", String::new())));
    assert_eq!(command("Reset", false), Some(("reset", String::new())));
    assert_eq!(command("!help loop", false), Some(("help", "loop".into())));
    assert_eq!(command("help me write an email", false), None);
    assert_eq!(command("reset my password", false), None);
    assert_eq!(command("new ideas for dinner", false), None);
    assert_eq!(command("status of the project?", false), None);
    // Prefix, as Discord's bare `model …` and `loop …`.
    assert_eq!(
        command("model codex", false),
        Some(("model", "codex".into()))
    );
    assert_eq!(
        command("/loop 30m digest", false),
        Some(("loop", "30m digest".into()))
    );
    assert_eq!(command("loops are nice", false), None);
    // Sigil, as Discord's `!journal` and `!loops`.
    assert_eq!(
        command("!journal hi", false),
        Some(("journal", "hi".into()))
    );
    assert_eq!(
        command("!loops stop 100", false),
        Some(("processes", "stop 100".into()))
    );
    assert_eq!(command("journal about my day", false), None);
    assert_eq!(command("ps aux shows nothing", false), None);
    // `cancel all` is a command; plain `cancel` is the existing path.
    assert_eq!(command("cancel all", false), Some(("cancel", "all".into())));
    assert_eq!(command("cancel", false), None);
    // An unknown `!word` is a command, answered with help.
    assert_eq!(
        command("!frobnicate now", false),
        Some(("<unknown>", String::new()))
    );
    assert_eq!(command("/tmp/foo is full", false), None);
}

// ---------------------------------------------------------------------------
// Help, unknown and malformed input
// ---------------------------------------------------------------------------

#[test]
fn help_is_generated_from_the_shared_registry() {
    let help = help_text();
    for c in OWNER_COMMANDS {
        assert!(
            help.contains(&format!("`{}", c.name)),
            "{} missing:\n{help}",
            c.name
        );
        if let SlackMapping::Blocked { issue, .. } = c.slack {
            assert!(help.contains(&format!("#{issue}")), "{help}");
        }
    }
    assert!(help.contains("/jarvis"), "{help}");
}

#[tokio::test]
async fn unknown_commands_and_malformed_arguments_get_help() {
    let f = Fixture::new();
    let c = f.commands();
    let idle = FakeControl::default();
    let unknown = c
        .execute(
            &recognize("!frobnicate", false).unwrap(),
            &CommandContext {
                owner: &owner(),
                conversation: &dm(),
                control: &idle,
                now_ms: 0,
            },
        )
        .await;
    assert!(
        unknown.contains("Unknown command `!frobnicate`"),
        "{unknown}"
    );
    assert!(unknown.contains("`model"), "help follows: {unknown}");
    let bad_model = run(&c, &dm(), &idle, "model set").await;
    assert!(bad_model.contains("Usage: `model"), "{bad_model}");
    let bad_loop = run(&c, &dm(), &idle, "loop pause").await;
    assert!(bad_loop.contains("Usage: `loop"), "{bad_loop}");
    let bad_ps = run(&c, &dm(), &idle, "processes nuke").await;
    assert!(bad_ps.contains("Usage: `processes"), "{bad_ps}");
    let help_model = run(&c, &dm(), &idle, "help model").await;
    assert!(help_model.contains("model [show"), "{help_model}");
    let help_unknown = run(&c, &dm(), &idle, "help nothing").await;
    assert!(
        help_unknown.contains("No command `nothing`"),
        "{help_unknown}"
    );
}

// ---------------------------------------------------------------------------
// Model selection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn model_selection_persists_per_conversation_across_restart() {
    let f = Fixture::new();
    let idle = FakeControl::default();
    let reply = run(&f.commands(), &dm(), &idle, "model set codex").await;
    assert!(
        reply.contains("codex") && reply.contains("this conversation"),
        "{reply}"
    );
    let other = run(&f.commands(), &thread(), &idle, "model qwen").await;
    assert!(other.contains("qwen"), "{other}");

    // A new command set and a new harness selection over the same file (a
    // daemon restart) see both choices, each on its own conversation.
    let restarted = f.commands();
    let show = run(&restarted, &dm(), &idle, "model").await;
    assert!(
        show.contains("codex") && show.contains("this conversation"),
        "{show}"
    );
    let select = slack_selection(f.selection.clone());
    assert_eq!(select(&dm()).unwrap(), Some(ProviderKind::Codex));
    assert_eq!(select(&thread()).unwrap(), Some(ProviderKind::Qwen));
    let other_dm = workspace().conversation("D00000009", None).unwrap();
    assert_eq!(select(&other_dm).unwrap(), None);

    let reset = run(&restarted, &dm(), &idle, "model reset").await;
    assert!(reset.contains("reset for this conversation"), "{reset}");
    assert_eq!(select(&dm()).unwrap(), None);
}

#[tokio::test]
async fn a_thread_without_its_own_model_uses_its_channel_then_the_daemon_default() {
    let f = Fixture::new();
    let idle = FakeControl::default();
    let c = f.commands();
    let select = slack_selection(f.selection.clone());
    run(&c, &dm(), &idle, "model set glm scope:default").await; // paused: refused
    assert_eq!(select(&thread()).unwrap(), None);
    run(&c, &dm(), &idle, "model set qwen scope:default").await;
    assert_eq!(select(&thread()).unwrap(), Some(ProviderKind::Qwen));
    run(&c, &channel(), &idle, "model set codex").await;
    assert_eq!(select(&thread()).unwrap(), Some(ProviderKind::Codex));
    let show = run(&c, &thread(), &idle, "model show").await;
    assert!(show.contains("codex") && show.contains("channel"), "{show}");
    run(&c, &thread(), &idle, "model claude").await;
    assert_eq!(select(&thread()).unwrap(), Some(ProviderKind::Claude));
}

#[tokio::test]
async fn a_paused_or_unknown_model_changes_nothing() {
    let f = Fixture::new();
    let idle = FakeControl::default();
    let c = f.commands();
    let paused = run(&c, &dm(), &idle, "model set glm").await;
    assert!(paused.contains("glm is paused"), "{paused}");
    let unknown = run(&c, &dm(), &idle, "model set gpt9").await;
    assert!(unknown.contains("Unknown model"), "{unknown}");
    assert_eq!(slack_selection(f.selection.clone())(&dm()).unwrap(), None);
}

#[tokio::test]
async fn switching_a_conversation_bound_to_another_session_is_refused_until_reset() {
    let f = Fixture::new();
    let idle = FakeControl::default();
    f.store
        .bind_surface_conversation(&NativeConversation {
            conversation: dm(),
            provider: "claude".into(),
            native_session_id: "session-claude-1".into(),
            cwd: "/wiki".into(),
            uncertain: false,
        })
        .unwrap();
    let c = f.commands();
    let refused = run(&c, &dm(), &idle, "model set codex").await;
    assert!(
        refused.contains("session-claude-1") && refused.contains("reset"),
        "{refused}"
    );
    assert_eq!(slack_selection(f.selection.clone())(&dm()).unwrap(), None);
    // The bound provider itself is fine.
    let same = run(&c, &dm(), &idle, "model set claude").await;
    assert!(same.contains("Model set to claude"), "{same}");
    // After reset the switch goes through.
    run(&c, &dm(), &idle, "reset").await;
    let switched = run(&c, &dm(), &idle, "model set codex").await;
    assert!(switched.contains("Model set to codex"), "{switched}");
}

// ---------------------------------------------------------------------------
// reset / cancel all / status
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reset_starts_a_new_session_and_refuses_while_a_request_runs() {
    let f = Fixture::new();
    let c = f.commands();
    f.store
        .bind_surface_conversation(&NativeConversation {
            conversation: dm(),
            provider: "claude".into(),
            native_session_id: "session-1".into(),
            cwd: "/wiki".into(),
            uncertain: true,
        })
        .unwrap();
    let busy = FakeControl::running(&dm());
    let refused = run(&c, &dm(), &busy, "reset").await;
    assert!(refused.contains("still running"), "{refused}");
    assert!(f.store.surface_conversation(&dm()).unwrap().is_some());

    let idle = FakeControl::default();
    let reply = run(&c, &dm(), &idle, "new").await;
    assert!(
        reply.contains("session-1") && reply.contains("new session"),
        "{reply}"
    );
    assert!(f.store.surface_conversation(&dm()).unwrap().is_none());
    let again = run(&c, &dm(), &idle, "reset").await;
    assert!(again.contains("no session yet"), "{again}");
}

#[tokio::test]
async fn cancel_all_stops_the_running_request_and_drops_the_queue() {
    let f = Fixture::new();
    let c = f.commands();
    for (i, id) in ["D00000001:1", "D00000001:2"].iter().enumerate() {
        f.store
            .record_inbound_event(
                &NewInboundEvent {
                    conversation: dm(),
                    event_id: (*id).into(),
                    kind: "message".into(),
                    occurred_at_ms: i as i64,
                    payload: "{}".into(),
                },
                0,
            )
            .unwrap();
    }
    let busy = FakeControl::running(&dm());
    let reply = run(&c, &dm(), &busy, "cancel all").await;
    assert!(reply.contains("Stopped the running request"), "{reply}");
    assert!(reply.contains("dropped 2 queued"), "{reply}");
    assert_eq!(busy.cancelled.lock().unwrap().len(), 1);
    assert_eq!(f.store.queued_inbound_events(&dm()).unwrap(), 0);
    let nothing = run(&c, &dm(), &FakeControl::default(), "cancel all").await;
    assert!(
        nothing.contains("Nothing is running or queued"),
        "{nothing}"
    );
}

#[tokio::test]
async fn status_reports_model_session_and_queue() {
    let f = Fixture::new();
    let c = f.commands();
    let idle = FakeControl::default();
    run(&c, &dm(), &idle, "model set codex").await;
    f.store
        .bind_surface_conversation(&NativeConversation {
            conversation: dm(),
            provider: "codex".into(),
            native_session_id: "codex-thread-7".into(),
            cwd: "/wiki".into(),
            uncertain: false,
        })
        .unwrap();
    let status = run(&c, &dm(), &FakeControl::running(&dm()), "status").await;
    assert!(status.contains("codex"), "{status}");
    assert!(status.contains("codex-thread-7"), "{status}");
    assert!(status.contains("Running: yes"), "{status}");
    assert!(status.contains("Queued: 0"), "{status}");
}

// ---------------------------------------------------------------------------
// Loops and reminders
// ---------------------------------------------------------------------------

fn loop_id(reply: &str) -> String {
    reply
        .split('`')
        .nth(1)
        .unwrap_or_else(|| panic!("no id in {reply:?}"))
        .to_string()
}

#[tokio::test]
async fn loops_are_created_for_this_slack_conversation_and_managed_by_id() {
    let f = Fixture::new();
    let c = f.commands();
    let idle = FakeControl::default();
    run(&c, &thread(), &idle, "model set codex").await;
    let created = run(&c, &thread(), &idle, "loop 30m check the build").await;
    assert!(
        created.contains("created") && created.contains("every 30m"),
        "{created}"
    );
    assert_eq!(
        f.parser.calls.load(Ordering::SeqCst),
        0,
        "deterministic grammar first"
    );
    let id = loop_id(&created);

    let rows = f
        .store
        .list_user_loops(&slack_loop_owner(&owner()))
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].channel, "slack");
    assert_eq!(rows[0].interval_secs, 1800);
    assert_eq!(rows[0].model_profile.as_deref(), Some("codex"));
    assert_eq!(parse_slack_loop_ref(&rows[0].channel_ref), Some(thread()));

    let list = run(&c, &dm(), &idle, "loop list").await;
    assert!(
        list.contains(&id) && list.contains("check the build"),
        "{list}"
    );

    let paused = run(&c, &dm(), &idle, &format!("loop pause {id}")).await;
    assert!(paused.contains("paused"), "{paused}");
    assert!(f.store.list_active_user_loops().unwrap().is_empty());
    let resumed = run(&c, &dm(), &idle, &format!("loop resume {id}")).await;
    assert!(resumed.contains("resumed"), "{resumed}");
    assert_eq!(f.store.list_active_user_loops().unwrap().len(), 1);
    let deleted = run(&c, &dm(), &idle, &format!("loop delete {id}")).await;
    assert!(deleted.contains("stopped"), "{deleted}");
    assert!(f
        .store
        .list_user_loops(&slack_loop_owner(&owner()))
        .unwrap()
        .is_empty());
    let missing = run(&c, &dm(), &idle, &format!("loop stop {id}")).await;
    assert!(missing.contains("no active loop"), "{missing}");
}

#[tokio::test]
async fn free_form_schedules_fall_back_to_the_model_parser_and_reminders_can_be_closed() {
    let f = Fixture::new();
    let c = f.commands();
    let idle = FakeControl::default();
    let created = run(
        &c,
        &dm(),
        &idle,
        "loop remind me each morning to read my inbox",
    )
    .await;
    assert!(created.contains("created"), "{created}");
    assert_eq!(f.parser.calls.load(Ordering::SeqCst), 1);
    let id = loop_id(&created);
    let garbage = run(&c, &dm(), &idle, "loop whenever you feel like it").await;
    assert!(garbage.contains("couldn't understand"), "{garbage}");

    // No reminder open yet.
    let early = run(&c, &dm(), &idle, &format!("loop ack {id}")).await;
    assert!(early.contains("no open reminder"), "{early}");
    // The scheduler opens a nag cycle on the first fire.
    assert!(f.store.open_nag_cycle(&id, 1_700_000_000_000).unwrap());
    let ack = run(&c, &dm(), &idle, &format!("loop ack {id}")).await;
    assert!(ack.contains("acknowledged"), "{ack}");
    assert!(f.store.list_active_user_loops().unwrap()[0]
        .nag_cycle_ms
        .is_none());
    assert!(f.store.open_nag_cycle(&id, 1_700_086_400_000).unwrap());
    let dismissed = run(&c, &dm(), &idle, &format!("loop dismiss {id}")).await;
    assert!(dismissed.contains("dismissed"), "{dismissed}");
}

struct AnswerRunner {
    runs: Mutex<Vec<(String, Option<String>)>>,
}

#[async_trait]
impl LoopRunner for AnswerRunner {
    async fn run_prompt(
        &self,
        _request_id: &str,
        owner: &str,
        prompt: &str,
        model: Option<&str>,
    ) -> anyhow::Result<String> {
        self.runs
            .lock()
            .unwrap()
            .push((owner.to_string(), model.map(str::to_string)));
        Ok(format!("answer for {prompt}"))
    }
}

/// Wraps a poster and wakes the test after each post.
struct Notifying {
    inner: Arc<dyn LoopPoster>,
    posted: Arc<tokio::sync::Notify>,
    count: AtomicUsize,
}

#[async_trait]
impl LoopPoster for Notifying {
    async fn post_to(&self, channel_ref: &str, body: &str) -> anyhow::Result<()> {
        let r = self.inner.post_to(channel_ref, body).await;
        self.count.fetch_add(1, Ordering::SeqCst);
        self.posted.notify_one();
        r
    }
    async fn post_reminder(
        &self,
        channel_ref: &str,
        body: &str,
        loop_id: &str,
        cycle_ms: i64,
    ) -> anyhow::Result<()> {
        let r = self
            .inner
            .post_reminder(channel_ref, body, loop_id, cycle_ms)
            .await;
        self.count.fetch_add(1, Ordering::SeqCst);
        self.posted.notify_one();
        r
    }
}

#[tokio::test(start_paused = true)]
async fn a_due_loop_fires_into_its_slack_conversation_on_the_scheduler_clock() {
    let f = Fixture::new();
    let c = f.commands();
    let idle = FakeControl::default();
    let created = run(&c, &thread(), &idle, "loop 5m check the build").await;
    let id = loop_id(&created);

    let runner = Arc::new(AnswerRunner {
        runs: Mutex::new(Vec::new()),
    });
    let posted = Arc::new(tokio::sync::Notify::new());
    let poster = Arc::new(Notifying {
        inner: Arc::new(SurfaceLoopPoster {
            slack: Some(Arc::new(SlackLoopPoster::new(Arc::clone(&f.store)))),
            other: None,
        }),
        posted: Arc::clone(&posted),
        count: AtomicUsize::new(0),
    });
    let scheduler = Arc::new(LoopScheduler::new(
        Arc::clone(&f.store),
        runner.clone(),
        poster.clone(),
    ));
    let shutdown = tokio_util::sync::CancellationToken::new();
    let task = tokio::spawn(scheduler.run(shutdown.clone()));
    // The first tick is immediate on the paused clock and the never-run
    // loop is due.
    posted.notified().await;
    // Two more scheduler ticks: the loop is not due again for 5 minutes.
    tokio::time::sleep(Duration::from_secs(61)).await;
    shutdown.cancel();
    task.await.unwrap().unwrap();
    assert_eq!(poster.count.load(Ordering::SeqCst), 1);

    let runs = runner.runs.lock().unwrap().clone();
    assert_eq!(runs, vec![(slack_loop_owner(&owner()), None)]);
    let sends = f
        .store
        .outbound_sends_with_key_prefix(&workspace().account(), "turn:loop:", &[])
        .unwrap();
    assert_eq!(sends.len(), 1, "{sends:?}");
    assert_eq!(sends[0].conversation, thread());
    assert!(
        sends[0].payload.contains("answer for check the build"),
        "{}",
        sends[0].payload
    );
    assert!(sends[0].payload.contains(&id), "{}", sends[0].payload);
    let row = &f
        .store
        .list_user_loops(&slack_loop_owner(&owner()))
        .unwrap()[0];
    assert_eq!(row.last_status.as_deref(), Some("ok"));
}

#[tokio::test]
async fn a_reminder_posted_to_slack_says_how_to_close_it() {
    let f = Fixture::new();
    let poster = SlackLoopPoster::new(Arc::clone(&f.store));
    poster
        .post_reminder(&slack_loop_ref(&dm()), "time to stretch", "loop-1", 42)
        .await
        .unwrap();
    let sends = f
        .store
        .outbound_sends_with_key_prefix(&workspace().account(), "turn:loop:", &[])
        .unwrap();
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0].conversation, dm());
    assert!(
        sends[0].payload.contains("loop ack loop-1"),
        "{}",
        sends[0].payload
    );
    assert!(
        sends[0].payload.contains("loop dismiss loop-1"),
        "{}",
        sends[0].payload
    );
}

struct Recording(Mutex<Vec<String>>);

#[async_trait]
impl LoopPoster for Recording {
    async fn post_to(&self, channel_ref: &str, _body: &str) -> anyhow::Result<()> {
        self.0.lock().unwrap().push(channel_ref.to_string());
        Ok(())
    }
}

#[tokio::test]
async fn loop_output_is_routed_by_destination_and_unwired_surfaces_never_run() {
    let f = Fixture::new();
    let discord = Arc::new(Recording(Mutex::new(Vec::new())));
    let routed = SurfaceLoopPoster {
        slack: Some(Arc::new(SlackLoopPoster::new(Arc::clone(&f.store)))),
        other: Some(discord.clone()),
    };
    routed.post_to("123456", "discord result").await.unwrap();
    routed
        .post_to(&slack_loop_ref(&dm()), "slack result")
        .await
        .unwrap();
    assert_eq!(*discord.0.lock().unwrap(), vec!["123456".to_string()]);
    let slack_only = SurfaceLoopPoster {
        slack: Some(Arc::new(SlackLoopPoster::new(Arc::clone(&f.store)))),
        other: None,
    };
    let err = slack_only.post_to("123456", "x").await.unwrap_err();
    assert!(err.to_string().contains("not configured"), "{err}");
    assert!(slack_only.post_to("slack:not-json", "x").await.is_err());

    // A loop whose surface is not wired in this daemon is not run at all:
    // no provider call for output nobody can receive.
    let inner = Arc::new(AnswerRunner {
        runs: Mutex::new(Vec::new()),
    });
    let gated = SurfaceGatedRunner {
        inner: inner.clone(),
        slack: true,
        discord: false,
    };
    let refused = gated.run_prompt("r1", "1234", "p", None).await.unwrap_err();
    assert!(refused.to_string().contains("Discord"), "{refused}");
    assert!(inner.runs.lock().unwrap().is_empty());
    gated
        .run_prompt("r2", &slack_loop_owner(&owner()), "p", None)
        .await
        .unwrap();
    assert_eq!(inner.runs.lock().unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// Journal, processes, voice
// ---------------------------------------------------------------------------

#[tokio::test]
async fn journal_saves_text_and_names_what_slack_cannot_do_yet() {
    let f = Fixture::new();
    let c = f.commands();
    let idle = FakeControl::default();
    let saved = run(&c, &dm(), &idle, "journal a calm, productive day").await;
    assert!(saved.contains("Saved to your journal"), "{saved}");
    assert_eq!(
        *f.journal.saved.lock().unwrap(),
        vec!["a calm, productive day"]
    );
    let usage = run(&c, &dm(), &idle, "journal").await;
    assert!(usage.contains("journal"), "{usage}");
    let done = run(&c, &dm(), &idle, "journal done Friday").await;
    assert!(done.contains("#1296"), "{done}");

    let mut deps = f.deps(FakeProcs {
        procs: vec![],
        self_pid: 1,
        parents: HashMap::new(),
        fail: false,
    });
    deps.journal = None;
    let unconfigured = SlackCommands::new(Arc::clone(&f.store), deps);
    assert_eq!(
        run(&unconfigured, &dm(), &idle, "journal hi").await,
        JOURNAL_NOT_CONFIGURED
    );
    // `done` is a Slack blocker whether or not the journal is configured.
    let done_unconfigured = run(&unconfigured, &dm(), &idle, "journal done").await;
    assert!(done_unconfigured.contains("#1296"), "{done_unconfigured}");
}

#[tokio::test]
async fn processes_are_listed_and_stopped_through_the_process_walker() {
    let f = Fixture::new();
    let c = f.commands();
    let idle = FakeControl::default();
    let list = run(&c, &dm(), &idle, "processes").await;
    assert!(
        list.contains("100") && list.contains("200") && list.contains("PID"),
        "{list}"
    );
    let stop = run(&c, &dm(), &idle, "processes stop 100").await;
    assert!(stop.contains("100") && stop.contains("stopped"), "{stop}");
    // `--all` spares this daemon's own ancestor chain (200).
    let all = run(&c, &dm(), &idle, "ps stop --all").await;
    assert!(all.contains("Stopping 1"), "{all}");
    assert_eq!(
        *f.signals.sent.lock().unwrap(),
        vec![(100, "TERM"), (100, "TERM")]
    );
}

#[tokio::test]
async fn an_unavailable_process_walker_is_reported_not_silently_empty() {
    let f = Fixture::new();
    let c = f.commands_with(FakeProcs {
        procs: vec![],
        self_pid: 1,
        parents: HashMap::new(),
        fail: true,
    });
    let reply = run(&c, &dm(), &FakeControl::default(), "processes").await;
    assert!(
        reply.contains("unavailable on this host") && reply.contains("ps: command not found"),
        "{reply}"
    );
    let stop_all = run(&c, &dm(), &FakeControl::default(), "processes stop --all").await;
    assert!(stop_all.contains("unavailable on this host"), "{stop_all}");
}

#[tokio::test]
async fn voice_names_the_live_voice_blocker() {
    let f = Fixture::new();
    let reply = run(&f.commands(), &dm(), &FakeControl::default(), "voice").await;
    assert!(reply.contains("#1298"), "{reply}");
}

// #1297 — `voice on|off|status`: spoken replies per conversation.
#[test]
fn voice_on_off_status_are_commands_in_plain_text_too() {
    assert_eq!(command("voice on", false), Some(("voice", "on".into())));
    assert_eq!(command("Voice OFF", false), Some(("voice", "OFF".into())));
    assert_eq!(
        command("voice status", false),
        Some(("voice", "status".into()))
    );
    assert_eq!(command("!voice on", false), Some(("voice", "on".into())));
    assert_eq!(command("voice on", true), Some(("voice", "on".into())));
    // A sentence about voice still goes to the agent.
    assert_eq!(command("voice notes are great", false), None);
    assert_eq!(command("voice on the phone was bad", false), None);
    assert_eq!(command("voice", false), None);
}

#[tokio::test]
async fn voice_sets_the_reply_mode_per_conversation_and_a_thread_inherits_its_channel() {
    use augmentagent_channel_slack::voice::reply::{reply_mode_for, ReplyMode};
    use augmentagent_channel_slack::voice::VoiceReadiness;
    let f = Fixture::new();
    let mut deps = f.deps(FakeProcs {
        procs: vec![],
        self_pid: 1,
        parents: HashMap::new(),
        fail: false,
    });
    deps.voice = Some(VoiceReadiness {
        stt: Ok("whisper-cpp".into()),
        tts: Err("DEEPGRAM_API_KEY is not set".into()),
    });
    let c = SlackCommands::new(Arc::clone(&f.store), deps);
    let idle = FakeControl::default();

    assert_eq!(
        reply_mode_for(&f.store, &channel()).unwrap(),
        ReplyMode::Text
    );
    let on = run(&c, &channel(), &idle, "voice on").await;
    assert!(on.contains("Spoken replies are on"), "{on}");
    // No provider: said at once, and answers stay text with a note.
    assert!(on.contains("DEEPGRAM_API_KEY is not set"), "{on}");
    assert_eq!(
        reply_mode_for(&f.store, &channel()).unwrap(),
        ReplyMode::Spoken
    );
    // A thread of that channel inherits it until it chooses for itself.
    assert_eq!(
        reply_mode_for(&f.store, &thread()).unwrap(),
        ReplyMode::Spoken
    );
    let off = run(&c, &thread(), &idle, "voice off").await;
    assert!(off.contains("off"), "{off}");
    assert_eq!(
        reply_mode_for(&f.store, &thread()).unwrap(),
        ReplyMode::Text
    );
    assert_eq!(
        reply_mode_for(&f.store, &channel()).unwrap(),
        ReplyMode::Spoken
    );
    // Each conversation is its own: the DM is untouched.
    assert_eq!(reply_mode_for(&f.store, &dm()).unwrap(), ReplyMode::Text);

    let status = run(&c, &channel(), &idle, "voice status").await;
    assert!(status.contains("Spoken replies: on"), "{status}");
    assert!(status.contains("whisper-cpp"), "{status}");
    assert!(status.contains("DEEPGRAM_API_KEY is not set"), "{status}");
    assert!(status.contains("#1298"), "{status}");
    let status = run(&c, &thread(), &idle, "voice").await;
    assert!(status.contains("Spoken replies: off"), "{status}");

    let bad = run(&c, &dm(), &idle, "voice loud").await;
    assert!(bad.contains("Usage: `voice"), "{bad}");
    assert_eq!(reply_mode_for(&f.store, &dm()).unwrap(), ReplyMode::Text);
}

#[test]
fn slack_loop_refs_round_trip_and_never_look_like_discord_ids() {
    for conversation in [dm(), thread(), channel()] {
        let r = slack_loop_ref(&conversation);
        assert!(r.starts_with("slack:"), "{r}");
        assert_eq!(parse_slack_loop_ref(&r), Some(conversation));
    }
    assert_eq!(parse_slack_loop_ref("123456"), None);
    let enterprise = SlackWorkspace::new(TEAM, Some("E00000001")).unwrap();
    let c = enterprise.conversation(DM, None).unwrap();
    assert_eq!(parse_slack_loop_ref(&slack_loop_ref(&c)), Some(c));
    // A turn ref is unaffected by any of this.
    let _ = SurfaceTurnRef::new(dm(), "t").unwrap();
}
