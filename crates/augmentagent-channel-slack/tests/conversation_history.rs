//! #1296 — Slack conversation history for the harness: bounded,
//! thread-aware, owner and agent only; and a history-in-prompt provider
//! (Qwen) gets it on Slack so it keeps multi-turn context.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use augmentagent_approval_discord::{AuditCtx, QueryHandler};
use augmentagent_channel_core::providers::ProviderKind;
use augmentagent_channel_slack::harness::SlackConversationHarness;
use augmentagent_channel_slack::history::{HistoryLimits, SlackConversationHistory};
use augmentagent_channel_slack::interactive::{
    slack_turn_id, SlackTurn, SlackTurnHandler, SlackWorkspaceRuntime,
};
use augmentagent_channel_slack::owner::{OwnerInputSource, SlackBotIdentity};
use augmentagent_channel_slack::surface::SlackWorkspace;
use augmentagent_channel_slack::transport::event::{parse_envelope_value, Envelope};
use augmentagent_channel_slack::transport::web::{
    AuthTest, ConversationInfo, HistoryQuery, PostEphemeral, PostMessage, PostedMessage,
    SlackHistoryMessage, SlackHistoryPage, SlackWebApi, UpdateMessage, UserInfo, ViewRef,
    WebApiError,
};
use augmentagent_store::Store;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

const TEAM: &str = "T0000001";
const OWNER: &str = "U000000A";
const STRANGER: &str = "U000000C";
const BOT_USER: &str = "U000000B";
const BOT_ID: &str = "B0000001";
const DM: &str = "D0000001";
const CONTROL: &str = "C0000001";
const NOW_SECS: i64 = 1_800_000_000;

fn ts(secs: i64, n: i64) -> String {
    format!("{secs}.{n:06}")
}

fn msg(ts: &str, user: Option<&str>, bot: bool, text: &str) -> Value {
    let mut m = json!({"type": "message", "ts": ts, "text": text});
    if let Some(u) = user {
        m["user"] = json!(u);
    }
    if bot {
        m["bot_id"] = json!(BOT_ID);
        m["user"] = json!(BOT_USER);
    }
    m
}

#[derive(Default)]
struct FakeHistory {
    /// Channel history (any order; returned newest first).
    history: Vec<Value>,
    /// Thread replies (returned oldest first).
    replies: Vec<Value>,
    calls: Mutex<Vec<String>>,
    fail: bool,
}

fn page(mut raw: Vec<Value>, newest_first: bool, limit: u32) -> SlackHistoryPage {
    raw.sort_by(|a, b| a["ts"].as_str().cmp(&b["ts"].as_str()));
    if newest_first {
        raw.reverse();
    }
    raw.truncate(limit as usize);
    SlackHistoryPage {
        messages: raw
            .into_iter()
            .map(|r| SlackHistoryMessage {
                ts: r["ts"].as_str().unwrap().into(),
                thread_ts: r["thread_ts"].as_str().map(str::to_string),
                text: r["text"].as_str().map(str::to_string),
                user: r["user"].as_str().map(str::to_string),
                bot_id: r["bot_id"].as_str().map(str::to_string),
                metadata: None,
                raw: r,
            })
            .collect(),
        has_more: false,
        next_cursor: None,
    }
}

fn unsupported<T>() -> Result<T, WebApiError> {
    Err(WebApiError::Unsupported("fake"))
}

#[async_trait]
impl SlackWebApi for FakeHistory {
    async fn post_message(&self, _: PostMessage) -> Result<PostedMessage, WebApiError> {
        unsupported()
    }
    async fn update_message(&self, _: UpdateMessage) -> Result<PostedMessage, WebApiError> {
        unsupported()
    }
    async fn delete_message(&self, _: &str, _: &str) -> Result<(), WebApiError> {
        unsupported()
    }
    async fn post_ephemeral(&self, _: PostEphemeral) -> Result<String, WebApiError> {
        unsupported()
    }
    async fn open_modal(&self, _: &str, _: Value) -> Result<ViewRef, WebApiError> {
        unsupported()
    }
    async fn update_modal(
        &self,
        _: &str,
        _: Option<&str>,
        _: Value,
    ) -> Result<ViewRef, WebApiError> {
        unsupported()
    }
    async fn add_reaction(&self, _: &str, _: &str, _: &str) -> Result<(), WebApiError> {
        unsupported()
    }
    async fn user_info(&self, _: &str) -> Result<UserInfo, WebApiError> {
        unsupported()
    }
    async fn conversation_info(&self, _: &str) -> Result<ConversationInfo, WebApiError> {
        unsupported()
    }
    async fn auth_test(&self) -> Result<AuthTest, WebApiError> {
        unsupported()
    }
    async fn open_direct_conversation(&self, _: &str) -> Result<String, WebApiError> {
        unsupported()
    }
    async fn conversations_history(
        &self,
        q: HistoryQuery,
    ) -> Result<SlackHistoryPage, WebApiError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("history {} limit={}", q.channel, q.limit));
        if self.fail {
            return Err(WebApiError::Slack {
                error: "channel_not_found".into(),
                warning: None,
            });
        }
        Ok(page(self.history.clone(), true, q.limit))
    }
    async fn conversations_replies(
        &self,
        q: HistoryQuery,
    ) -> Result<SlackHistoryPage, WebApiError> {
        self.calls.lock().unwrap().push(format!(
            "replies {} {}",
            q.channel,
            q.thread_ts.as_deref().unwrap_or("-")
        ));
        Ok(page(self.replies.clone(), false, q.limit))
    }
}

fn runtime(web: Arc<FakeHistory>) -> SlackWorkspaceRuntime {
    SlackWorkspaceRuntime {
        workspace: SlackWorkspace::new(TEAM, None).unwrap(),
        web: web as Arc<dyn SlackWebApi>,
        bot: SlackBotIdentity {
            bot_user_id: Some(BOT_USER.into()),
            bot_id: Some(BOT_ID.into()),
            app_id: Some("A0000001".into()),
        },
    }
}

fn provider(web: Arc<FakeHistory>, limits: HistoryLimits) -> SlackConversationHistory {
    SlackConversationHistory::new(&[runtime(web)])
        .with_limits(limits)
        .with_clock(Arc::new(|| NOW_SECS * 1000))
}

#[tokio::test]
async fn a_thread_turn_reads_that_thread_bounded_and_owner_only() {
    let parent = ts(NOW_SECS - 600, 1);
    let mut replies = vec![msg(&parent, Some(OWNER), false, "plan the launch")];
    for i in 0..20 {
        let at = ts(NOW_SECS - 500 + i * 10, 1);
        replies.push(if i % 2 == 0 {
            msg(&at, None, true, &format!("agent {i}"))
        } else {
            msg(&at, Some(OWNER), false, &format!("owner {i}"))
        });
    }
    replies.push(msg(
        &ts(NOW_SECS - 250, 5),
        Some(STRANGER),
        false,
        "a stranger chimes in",
    ));
    let current = ts(NOW_SECS - 100, 1);
    replies.push(msg(&current, Some(OWNER), false, "and now?"));
    replies.push(msg(
        &ts(NOW_SECS - 50, 1),
        Some(OWNER),
        false,
        "after the current one",
    ));
    let web = Arc::new(FakeHistory {
        replies,
        ..FakeHistory::default()
    });
    let limits = HistoryLimits {
        max_messages: 6,
        ..HistoryLimits::default()
    };
    let out = provider(Arc::clone(&web), limits)
        .transcript(TEAM, CONTROL, Some(&parent), Some(&current), OWNER)
        .await;
    assert!(
        out.starts_with("<conversation_history>\n") && out.ends_with("</conversation_history>")
    );
    let lines: Vec<&str> = out.lines().filter(|l| l.contains(": ")).collect();
    assert_eq!(lines.len(), 6, "{out}");
    assert_eq!(lines[0], "assistant: agent 14");
    assert_eq!(lines[5], "user: owner 19");
    assert!(!out.contains("stranger"), "only the owner and the agent");
    assert!(!out.contains("and now?"), "not the current message");
    assert!(!out.contains("after the current"));
    assert_eq!(
        web.calls.lock().unwrap().clone(),
        vec![format!("replies {CONTROL} {parent}")]
    );
}

#[tokio::test]
async fn a_dm_turn_reads_the_dm_within_the_age_and_size_caps() {
    let mut history = vec![msg(
        &ts(NOW_SECS - 3 * 3600, 1),
        Some(OWNER),
        false,
        "too old",
    )];
    history.push(msg(
        &ts(NOW_SECS - 600, 1),
        Some(OWNER),
        false,
        &"x".repeat(80),
    ));
    history.push(msg(&ts(NOW_SECS - 500, 1), None, true, "short answer"));
    history.push(msg(&ts(NOW_SECS - 400, 1), Some(OWNER), false, "follow-up"));
    let web = Arc::new(FakeHistory {
        history,
        ..FakeHistory::default()
    });
    let limits = HistoryLimits {
        max_messages: 30,
        max_age_ms: 2 * 3600 * 1000,
        char_cap: 60,
    };
    let out = provider(Arc::clone(&web), limits)
        .transcript(TEAM, DM, None, Some(&ts(NOW_SECS, 1)), OWNER)
        .await;
    assert!(!out.contains("too old"));
    assert!(
        !out.contains("xxxx"),
        "the oldest entry is dropped over the cap"
    );
    assert!(
        out.contains("assistant: short answer\nuser: follow-up\n"),
        "{out}"
    );
    assert_eq!(
        web.calls.lock().unwrap().clone(),
        vec![format!("history {DM} limit=31")]
    );
}

#[tokio::test]
async fn a_failed_fetch_is_an_empty_history() {
    let web = Arc::new(FakeHistory {
        fail: true,
        ..FakeHistory::default()
    });
    let out = provider(web, HistoryLimits::default())
        .transcript(TEAM, DM, None, None, OWNER)
        .await;
    assert_eq!(out, "");
}

/// Records what the agent was asked.
#[derive(Default)]
struct RecordingAgent(Mutex<Vec<String>>);

#[async_trait]
impl QueryHandler for RecordingAgent {
    async fn answer(&self, _ctx: &AuditCtx, question: &str) -> anyhow::Result<String> {
        self.0.lock().unwrap().push(question.to_string());
        Ok("ok".into())
    }
}

fn dm_turn(current: &str, text: &str) -> SlackTurn {
    let ws = SlackWorkspace::new(TEAM, None).unwrap();
    let Envelope::Event(envelope) = parse_envelope_value(json!({
        "type": "events_api", "envelope_id": "env-1",
        "payload": {"team_id": TEAM, "event_id": "Ev1",
                    "event": {"type": "message", "channel": DM, "channel_type": "im",
                              "user": OWNER, "text": text, "ts": current}},
    }))
    .unwrap() else {
        panic!("event")
    };
    let event_id = format!("{DM}:{current}");
    SlackTurn {
        event_id: event_id.clone(),
        attempt: 1,
        owner: ws.owner(OWNER).unwrap(),
        conversation: Some(ws.conversation(DM, None).unwrap()),
        source: OwnerInputSource::Message,
        text: text.into(),
        envelope: *envelope,
        session: Some(ws.conversation(DM, None).unwrap()),
        turn_id: slack_turn_id(&ws.account(), &event_id),
        prompt: text.into(),
        inbound_dir: None,
        cancel: CancellationToken::new(),
    }
}

#[tokio::test]
async fn a_history_in_prompt_provider_keeps_multi_turn_context_on_slack() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("data.db")).unwrap());
    let web = Arc::new(FakeHistory {
        history: vec![
            msg(
                &ts(NOW_SECS - 120, 1),
                Some(OWNER),
                false,
                "my flight is UA 100 on friday",
            ),
            msg(&ts(NOW_SECS - 110, 1), None, true, "Noted: UA 100, Friday."),
        ],
        ..FakeHistory::default()
    });
    let agent = Arc::new(RecordingAgent::default());
    let harness =
        SlackConversationHarness::new(Arc::clone(&store), agent.clone(), dir.path().to_path_buf())
            .with_selection(Arc::new(|_| Ok(Some(ProviderKind::Qwen))))
            .with_history(Arc::new(provider(web, HistoryLimits::default())));
    let reply = harness
        .handle_turn(&dm_turn(&ts(NOW_SECS - 10, 1), "which day was it?"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reply.text, "ok");
    let asked = agent.0.lock().unwrap().clone();
    assert_eq!(asked.len(), 1);
    assert!(
        asked[0].contains("user: my flight is UA 100 on friday\nassistant: Noted: UA 100, Friday."),
        "{}",
        asked[0]
    );
    assert!(asked[0].ends_with("user's current message:\nwhich day was it?"));
}
