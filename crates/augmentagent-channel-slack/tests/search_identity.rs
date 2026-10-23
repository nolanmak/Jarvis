//! #1296 — Slack history is searchable through the shared message search
//! (`augmentagent messages search`, the agent's `search_messages` tool) with
//! correct people and channel identity: sender resolved through the wiki,
//! channel by name (following renames), DMs as DMs with their counterpart,
//! the message's own Slack time, and deleted messages gone.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use augmentagent_approval_discord::{ApprovalBroker, ApprovalError};
use augmentagent_channel_core::{Reasoner, ReasonerOpts};
use augmentagent_channel_slack::channel::PollOutcome;
use augmentagent_channel_slack::ingest::LiveIngest;
use augmentagent_channel_slack::transport::event::{parse_envelope_value, Envelope, EventEnvelope};
use augmentagent_channel_slack::{SlackChannel, SlackChannelConfig};
use augmentagent_messages::query::{search, SearchResponse};
use augmentagent_store::{ChannelSubscription, Email, Store, SubscriptionMode};
use serde_json::json;

const TEAM: &str = "T0000001";
const CHAN: &str = "C0000001";
const DM: &str = "D0000002";
const OWNER: &str = "U000000A";
const ALICE: &str = "U000000B";
/// 2027-01-15T08:00:00Z.
const TS: &str = "1800000000.000100";

struct Skip;

#[async_trait]
impl Reasoner for Skip {
    async fn call(&self, _: &ReasonerOpts, _: &str) -> anyhow::Result<String> {
        Ok(r#"{"decision":"skip","reason":"test"}"#.into())
    }
}

struct NoBroker;

#[async_trait]
impl ApprovalBroker for NoBroker {
    async fn post_approval(&self, _: &str, _: &Email, _: &str) -> Result<(), ApprovalError> {
        Ok(())
    }
    async fn post_flag_notice(&self, _: &Email, _: &str) -> Result<(), ApprovalError> {
        Ok(())
    }
}

fn person(wiki: &Path, slug: &str, title: &str, slack: &str) {
    std::fs::create_dir_all(wiki.join("people")).unwrap();
    std::fs::write(
        wiki.join("people").join(format!("{slug}.md")),
        format!("---\nkind: person\nidentities:\n  slack: {slack}\n---\n# {title}\n"),
    )
    .unwrap();
}

struct World {
    _dir: tempfile::TempDir,
    store: Arc<Store>,
    channel: Arc<SlackChannel<Skip>>,
    live: LiveIngest<Skip>,
    general: ChannelSubscription,
    dm: ChannelSubscription,
}

fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("data.db")).unwrap());
    store
        .upsert_slack_workspace(TEAM, "Example Co", "entity", "conn", OWNER)
        .unwrap();
    let general = store
        .upsert_subscription(
            "slack",
            CHAN,
            "#general",
            SubscriptionMode::Digest,
            Some(TEAM),
        )
        .unwrap();
    let dm = store
        .upsert_subscription(
            "slack",
            DM,
            "DM with Alice Example",
            SubscriptionMode::Digest,
            Some(TEAM),
        )
        .unwrap();
    let wiki = dir.path().join("wiki");
    person(&wiki, "alice-example", "Alice Example", ALICE);
    augmentagent_messages::people::resolve_people(&store, &wiki).unwrap();
    let channel = Arc::new(SlackChannel::new(
        Arc::clone(&store),
        Arc::new(Skip),
        Arc::new(NoBroker) as Arc<dyn ApprovalBroker>,
        SlackChannelConfig::default(),
        None,
    ));
    let live = LiveIngest::new(Arc::clone(&channel));
    World {
        _dir: dir,
        store,
        channel,
        live,
        general,
        dm,
    }
}

fn envelope(id: &str, event: serde_json::Value) -> EventEnvelope {
    match parse_envelope_value(json!({
        "type": "events_api", "envelope_id": id,
        "payload": {"team_id": TEAM, "event_id": format!("Ev{id}"), "event": event},
    }))
    .unwrap()
    {
        Envelope::Event(e) => *e,
        other => panic!("{other:?}"),
    }
}

impl World {
    fn search(&self, q: &str) -> SearchResponse {
        augmentagent_messages::drain(&self.store, 100, Duration::ZERO).unwrap();
        self.store
            .with_conn(|c| Ok(search(c, q, None, 0).unwrap()))
            .unwrap()
    }
}

#[tokio::test]
async fn a_slack_channel_message_is_found_by_person_and_channel() {
    let w = world();
    // Stored by the poll (Composio gives a username), like before #1296.
    let page = vec![serde_json::from_value(json!({
        "type": "message", "ts": TS, "user": ALICE, "username": "alice",
        "text": "the quarterly launch checklist is ready",
    }))
    .unwrap()];
    let mut out = PollOutcome::default();
    w.channel
        .ingest_polled(&w.general, TEAM, OWNER, page, &mut out)
        .await
        .unwrap();

    let r = w.search("from:alice channel:general checklist");
    assert!(r.ambiguous.is_empty() && r.unresolved.is_empty(), "{r:?}");
    assert_eq!(r.hits.len(), 1, "{r:?}");
    let hit = &r.hits[0];
    assert_eq!(hit.message_id, format!("{CHAN}:{TS}"));
    assert_eq!(hit.platform, "slack");
    assert_eq!(hit.conv_kind, "channel");
    assert_eq!(hit.conversation_title.as_deref(), Some("#general"));
    assert_eq!(hit.container.as_deref(), Some("Example Co"));
    assert_eq!(hit.sender_handle, format!("slack:{ALICE}"));
    assert_eq!(hit.person.as_deref(), Some("alice-example"));
    assert_eq!(
        hit.timestamp, "2027-01-15T08:00:00+00:00",
        "Slack's own time"
    );
    // Kind and workspace operators.
    assert_eq!(w.search("is:channel in:slack checklist").hits.len(), 1);
    assert_eq!(w.search("server:\"Example Co\" checklist").hits.len(), 1);
    assert!(
        w.search("is:dm checklist").hits.is_empty(),
        "a channel is not a DM"
    );

    // A rename moves the channel name in search; the ID is unchanged.
    let rename = envelope(
        "rename",
        json!({"type": "channel_rename", "channel": {"id": CHAN, "name": "launch"}}),
    );
    w.live.handle(&rename).await.unwrap();
    let r = w.search("channel:launch checklist");
    assert_eq!(r.hits.len(), 1);
    assert_eq!(r.hits[0].message_id, format!("{CHAN}:{TS}"));
    assert!(w.search("channel:general checklist").hits.is_empty());

    // A deleted message is no longer found.
    let delete = envelope(
        "del",
        json!({"type": "message", "subtype": "message_deleted", "channel": CHAN, "deleted_ts": TS}),
    );
    w.live.handle(&delete).await.unwrap();
    assert!(w.search("checklist").hits.is_empty());
}

#[tokio::test]
async fn a_slack_dm_received_live_is_a_dm_with_that_person() {
    let w = world();
    let message = envelope(
        "dm",
        json!({"type": "message", "channel": DM, "channel_type": "im", "user": ALICE,
               "text": "dinner at the harbor on thursday?", "ts": TS}),
    );
    w.live.handle(&message).await.unwrap();
    let r = w.search("with:\"Alice Example\" harbor");
    assert_eq!(r.hits.len(), 1, "{r:?}");
    let hit = &r.hits[0];
    assert_eq!(hit.conv_kind, "dm");
    assert_eq!(
        hit.conversation_title.as_deref(),
        Some("DM with Alice Example")
    );
    assert_eq!(hit.person.as_deref(), Some("alice-example"));
    assert_eq!(w.search("is:dm from:alice-example harbor").hits.len(), 1);
    let _ = &w.dm;
}
