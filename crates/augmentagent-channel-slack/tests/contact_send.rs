//! #1290 — messages to Slack contacts, on the real dispatch path: the
//! store's claim, the send-target table, the per-action send ledger and the
//! Composio user connection (a local recording fake here). Every send is
//! made as the owner's own Slack account; nothing here can send as the app
//! bot. Temporary store and wiki; ids, people and text are synthetic.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use augmentagent_approval_discord::ApprovalActionOutcome;
use augmentagent_channel_slack::contact::compose::{
    compose, resolve_recipient, ComposeOutcome, Recipient, Resolution,
};
use augmentagent_channel_slack::contact::{
    approve_contact_message, ingested_reply_target, reply_target, send_contact_message,
    ContactClaim, ContactSendApi, ContactSendError, OutgoingContactMessage, PostedContactMessage,
};
use augmentagent_store::slack_contact::{
    SlackConversationKind, SlackSendIdentity, SlackSendStatus,
};
use augmentagent_store::{ActionStatus, ActionWithEmail, Email, Store, SubscriptionMode};

const TEAM: &str = "T00000009";
const OWNER: &str = "U00000009";
const CHANNEL: &str = "C00000009";
const PARENT_TS: &str = "1700000000.000100";

// ---------------------------------------------------------------------------
// Fake Composio user connection
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum Script {
    /// Posted and acknowledged.
    Ok,
    /// Refused: nothing posted.
    Refuse,
    /// Posted, but the response was lost (timeout after Slack accepted).
    LandedButLost,
    /// Timed out before Slack saw it.
    LostBeforeSlack,
    /// Posted; Slack attributes it to an app.
    AsApp,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Landed {
    channel: String,
    thread_ts: Option<String>,
    user: String,
    text: String,
    ts: String,
}

#[derive(Default)]
struct FakeComposio {
    owner: Option<String>,
    scripts: Mutex<VecDeque<Script>>,
    /// Every post request, whether or not it landed.
    requests: Mutex<Vec<OutgoingContactMessage>>,
    /// What is actually in Slack.
    landed: Mutex<Vec<Landed>>,
    history_fails: Mutex<bool>,
    history_lookups: Mutex<u32>,
}

impl FakeComposio {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            owner: Some(OWNER.into()),
            ..Self::default()
        })
    }

    fn script(&self, s: Script) {
        self.scripts.lock().unwrap().push_back(s);
    }

    fn requests(&self) -> Vec<OutgoingContactMessage> {
        self.requests.lock().unwrap().clone()
    }

    fn landed(&self) -> Vec<Landed> {
        self.landed.lock().unwrap().clone()
    }

    fn land(&self, m: &OutgoingContactMessage, user: &str) -> String {
        let mut landed = self.landed.lock().unwrap();
        let ts = format!("1700000100.{:06}", landed.len() + 1);
        let channel = if m.channel.starts_with('U') {
            "D0000000A".to_string()
        } else {
            m.channel.clone()
        };
        landed.push(Landed {
            channel,
            thread_ts: m.thread_ts.clone(),
            user: user.into(),
            text: m.text.clone(),
            ts: ts.clone(),
        });
        ts
    }
}

#[async_trait]
impl ContactSendApi for FakeComposio {
    fn owner_user_id(&self) -> Option<String> {
        self.owner.clone()
    }

    async fn post_as_owner(
        &self,
        m: &OutgoingContactMessage,
    ) -> Result<PostedContactMessage, ContactSendError> {
        self.requests.lock().unwrap().push(m.clone());
        let script = self
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Script::Ok);
        let owner = self.owner.clone().unwrap_or_default();
        match script {
            Script::Ok | Script::AsApp => {
                let ts = self.land(m, &owner);
                let channel = self.landed().last().unwrap().channel.clone();
                let app = matches!(script, Script::AsApp);
                Ok(PostedContactMessage {
                    channel,
                    ts,
                    user: Some(owner),
                    bot_id: app.then(|| "B0000000Z".to_string()),
                })
            }
            Script::Refuse => Err(ContactSendError::Rejected("not_in_channel".into())),
            Script::LandedButLost => {
                self.land(m, &owner);
                Err(ContactSendError::Unknown("timed out".into()))
            }
            Script::LostBeforeSlack => Err(ContactSendError::Unknown("timed out".into())),
        }
    }

    async fn find_owner_message(
        &self,
        channel: &str,
        thread_ts: Option<&str>,
        _oldest_ts: &str,
        owner_user_id: &str,
        text: &str,
    ) -> Result<Option<String>, ContactSendError> {
        *self.history_lookups.lock().unwrap() += 1;
        if *self.history_fails.lock().unwrap() {
            return Err(ContactSendError::Unknown("history unavailable".into()));
        }
        Ok(self
            .landed()
            .into_iter()
            .find(|l| {
                l.channel == channel
                    && l.thread_ts.as_deref() == thread_ts
                    && l.user == owner_user_id
                    && l.text == text
            })
            .map(|l| l.ts))
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

struct Fx {
    _dir: tempfile::TempDir,
    store: Arc<Store>,
    wiki: std::path::PathBuf,
}

fn page(wiki: &Path, slug: &str, title: &str, identities: &str) {
    let dir = wiki.join("people");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{slug}.md")),
        format!("---\nkind: person\nidentities:\n{identities}---\n# {title}\n"),
    )
    .unwrap();
}

fn fx() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("state dir ü");
    std::fs::create_dir_all(&root).unwrap();
    let store = Arc::new(Store::open(root.join("data.db")).unwrap());
    store
        .upsert_slack_workspace(TEAM, "Contacts Example", "entity-test", "conn-test", OWNER)
        .unwrap();
    let wiki = root.join("wiki");
    page(
        &wiki,
        "alice-example",
        "Alice Example",
        "  slack: U0000000A\n",
    );
    page(&wiki, "alex-one", "Alex One", "  slack: U0000000B\n");
    page(&wiki, "alex-two", "Alex Two", "  slack: U0000000C\n");
    page(
        &wiki,
        "bob-noslack",
        "Bob Noslack",
        "  email: bob@example.com\n",
    );
    store
        .upsert_subscription(
            "slack",
            CHANNEL,
            "#general",
            SubscriptionMode::Priority,
            Some(TEAM),
        )
        .unwrap();
    Fx {
        _dir: dir,
        store,
        wiki,
    }
}

/// A drafted reply to an ingested message, stored the way the Slack
/// channel stores it (email row, send target, pending action).
fn pending_reply(
    fx: &Fx,
    channel: &str,
    display: &str,
    thread_ts: Option<&str>,
    draft: &str,
) -> String {
    let ts = "1700000000.000200";
    let email = Email {
        message_id: format!("{channel}:{ts}"),
        thread_id: Some(channel.into()),
        from: "Contact Example <slack:U00000077>".into(),
        to: String::new(),
        cc: String::new(),
        attachments: Vec::new(),
        subject: String::new(),
        body: "Are you free next week?".into(),
        date: ts.into(),
        account_entity_id: Some(format!("slack:team:{TEAM}")),
        platform: "slack".into(),
        kind: "dm".into(),
    };
    fx.store.upsert_email(&email).unwrap();
    let target = ingested_reply_target(&email.message_id, TEAM, channel, display, ts, thread_ts);
    fx.store.record_slack_send_target(&target).unwrap();
    fx.store
        .log_action(
            &email.message_id,
            email.thread_id.as_deref(),
            &email.from,
            &email.subject,
            Some(&email.body),
            Some(draft),
            ActionStatus::Pending,
        )
        .unwrap()
}

fn row(fx: &Fx, id: &str) -> ActionWithEmail {
    fx.store.get_action_with_email(id).unwrap().unwrap()
}

async fn approve(fx: &Fx, api: &Arc<FakeComposio>, id: &str) -> ApprovalActionOutcome {
    let action = row(fx, id);
    approve_contact_message(
        &fx.store,
        Some(api.as_ref() as &dyn ContactSendApi),
        &action,
        "slack",
    )
    .await
}

fn status_source(fx: &Fx, id: &str) -> Option<String> {
    fx.store.action_status_source(id).unwrap()
}

// ---------------------------------------------------------------------------
// Replies
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_approved_channel_reply_lands_in_the_thread_as_the_owner_and_is_recorded() {
    let fx = fx();
    let api = FakeComposio::new();
    let id = pending_reply(
        &fx,
        CHANNEL,
        "#general",
        Some(PARENT_TS),
        "Sure — Tuesday works.",
    );

    let out = approve(&fx, &api, &id).await;
    assert!(matches!(out, ApprovalActionOutcome::Approved), "{out:?}");
    let landed = api.landed();
    assert_eq!(landed.len(), 1);
    assert_eq!(landed[0].channel, CHANNEL);
    assert_eq!(landed[0].thread_ts.as_deref(), Some(PARENT_TS));
    assert_eq!(landed[0].user, OWNER);
    assert_eq!(landed[0].text, "Sure — Tuesday works.");

    let a = row(&fx, &id);
    assert_eq!(a.action.status, "sent");
    assert_eq!(status_source(&fx, &id).as_deref(), Some("slack"));
    let sent = fx.store.slack_contact_send(&id).unwrap().unwrap();
    assert_eq!(sent.status, SlackSendStatus::Sent);
    assert_eq!(sent.identity, SlackSendIdentity::OwnerUser);
    assert_eq!(sent.sender_user_id, OWNER);
    assert_eq!(sent.observed_user.as_deref(), Some(OWNER));
    assert_eq!(sent.thread_ts.as_deref(), Some(PARENT_TS));
    let self_id = format!("slack:{CHANNEL}:{}", landed[0].ts);
    assert_eq!(
        fx.store
            .self_sent_message_platform(&self_id)
            .unwrap()
            .as_deref(),
        Some("slack"),
        "the send is recorded as a Slack self-send, not a Gmail one"
    );
}

#[tokio::test]
async fn a_top_level_channel_message_is_answered_in_its_own_thread() {
    let fx = fx();
    let api = FakeComposio::new();
    let id = pending_reply(&fx, CHANNEL, "#general", None, "On it.");
    assert!(matches!(
        approve(&fx, &api, &id).await,
        ApprovalActionOutcome::Approved
    ));
    assert_eq!(
        api.landed()[0].thread_ts.as_deref(),
        Some("1700000000.000200"),
        "a reply in a channel threads under the message it answers"
    );
}

#[tokio::test]
async fn a_dm_reply_goes_top_level_and_a_legacy_row_still_reaches_its_conversation() {
    let fx = fx();
    let api = FakeComposio::new();
    let dm = pending_reply(&fx, "D00000077", "DM with U00000077", None, "Yes!");
    assert!(matches!(
        approve(&fx, &api, &dm).await,
        ApprovalActionOutcome::Approved
    ));
    assert_eq!(api.landed()[0].channel, "D00000077");
    assert_eq!(api.landed()[0].thread_ts, None);

    // A row drafted before #1290 has no send target: its conversation is the
    // email's thread id and the reply goes top level, as before.
    let email = Email {
        message_id: "slack:C00000008:1700000000.000300".into(),
        thread_id: Some("C00000008".into()),
        from: "Contact Example <slack:U00000077>".into(),
        to: String::new(),
        cc: String::new(),
        attachments: Vec::new(),
        subject: String::new(),
        body: "hi".into(),
        date: String::new(),
        account_entity_id: Some(format!("slack:team:{TEAM}")),
        platform: "slack".into(),
        kind: "dm".into(),
    };
    fx.store.upsert_email(&email).unwrap();
    let legacy = fx
        .store
        .log_action(
            &email.message_id,
            email.thread_id.as_deref(),
            &email.from,
            "",
            Some("hi"),
            Some("Hello"),
            ActionStatus::Pending,
        )
        .unwrap();
    let t = reply_target(&fx.store, &row(&fx, &legacy).email).unwrap();
    assert_eq!(
        (t.channel_id.as_str(), t.thread_ts.as_deref()),
        ("C00000008", None)
    );
    assert!(matches!(
        approve(&fx, &api, &legacy).await,
        ApprovalActionOutcome::Approved
    ));
    assert_eq!(api.landed()[1].channel, "C00000008");
    assert_eq!(api.landed()[1].thread_ts, None);
}

#[tokio::test]
async fn two_approvals_at_once_send_once() {
    let fx = fx();
    let api = FakeComposio::new();
    let id = pending_reply(&fx, CHANNEL, "#general", Some(PARENT_TS), "Once only.");
    let (a, b) = tokio::join!(approve(&fx, &api, &id), approve(&fx, &api, &id));
    let approved = [&a, &b]
        .iter()
        .filter(|o| matches!(o, ApprovalActionOutcome::Approved))
        .count();
    assert_eq!(approved, 1, "{a:?} / {b:?}");
    assert_eq!(api.requests().len(), 1);
    // And later, on any surface.
    assert!(matches!(
        approve(&fx, &api, &id).await,
        ApprovalActionOutcome::AlreadyResolved { .. }
    ));
    assert_eq!(api.requests().len(), 1);
}

#[tokio::test]
async fn a_refused_send_then_a_retry_sends_exactly_once() {
    let fx = fx();
    let api = FakeComposio::new();
    let id = pending_reply(&fx, CHANNEL, "#general", Some(PARENT_TS), "Retry me.");
    api.script(Script::Refuse);
    match approve(&fx, &api, &id).await {
        ApprovalActionOutcome::Failed { message } => {
            assert!(message.contains("not_in_channel"), "{message}");
            assert!(message.contains("nothing was sent"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(row(&fx, &id).action.status, "error");
    assert_eq!(
        fx.store.slack_contact_send(&id).unwrap().unwrap().status,
        SlackSendStatus::Failed
    );
    assert!(api.landed().is_empty());

    // The owner approves again (the card offers it): one send.
    assert!(matches!(
        approve(&fx, &api, &id).await,
        ApprovalActionOutcome::Approved
    ));
    assert_eq!(api.landed().len(), 1);
    assert_eq!(row(&fx, &id).action.status, "sent");
    assert_eq!(
        fx.store.slack_contact_send(&id).unwrap().unwrap().attempts,
        2
    );
    // A third approval sends nothing.
    assert!(matches!(
        approve(&fx, &api, &id).await,
        ApprovalActionOutcome::AlreadyResolved { .. }
    ));
    assert_eq!(api.requests().len(), 2);
    assert_eq!(api.landed().len(), 1);
}

#[tokio::test]
async fn a_lost_response_that_landed_is_found_on_retry_and_not_sent_again() {
    let fx = fx();
    let api = FakeComposio::new();
    let id = pending_reply(&fx, CHANNEL, "#general", Some(PARENT_TS), "Landed anyway.");
    api.script(Script::LandedButLost);
    match approve(&fx, &api, &id).await {
        ApprovalActionOutcome::Failed { message } => {
            assert!(message.contains("may or may not"), "{message}")
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        fx.store.slack_contact_send(&id).unwrap().unwrap().status,
        SlackSendStatus::Unknown
    );
    assert_eq!(api.landed().len(), 1, "Slack has it");

    assert!(matches!(
        approve(&fx, &api, &id).await,
        ApprovalActionOutcome::Approved
    ));
    assert_eq!(
        api.requests().len(),
        1,
        "the retry checked and did not post again"
    );
    assert_eq!(api.landed().len(), 1);
    assert_eq!(*api.history_lookups.lock().unwrap(), 1);
    let sent = fx.store.slack_contact_send(&id).unwrap().unwrap();
    assert_eq!(sent.status, SlackSendStatus::Sent);
    assert_eq!(sent.remote_ts.as_deref(), Some(api.landed()[0].ts.as_str()));
    assert_eq!(row(&fx, &id).action.status, "sent");
}

#[tokio::test]
async fn a_timeout_that_did_not_land_is_sent_once_on_retry() {
    let fx = fx();
    let api = FakeComposio::new();
    let id = pending_reply(&fx, CHANNEL, "#general", Some(PARENT_TS), "Try again.");
    api.script(Script::LostBeforeSlack);
    assert!(matches!(
        approve(&fx, &api, &id).await,
        ApprovalActionOutcome::Failed { .. }
    ));
    assert!(api.landed().is_empty());
    assert!(matches!(
        approve(&fx, &api, &id).await,
        ApprovalActionOutcome::Approved
    ));
    assert_eq!(api.landed().len(), 1);
    assert_eq!(*api.history_lookups.lock().unwrap(), 1);
}

#[tokio::test]
async fn when_the_conversation_cannot_be_checked_the_retry_asks_before_sending_again() {
    let fx = fx();
    let api = FakeComposio::new();
    let id = pending_reply(&fx, CHANNEL, "#general", Some(PARENT_TS), "Careful.");
    api.script(Script::LandedButLost);
    assert!(matches!(
        approve(&fx, &api, &id).await,
        ApprovalActionOutcome::Failed { .. }
    ));
    *api.history_fails.lock().unwrap() = true;
    match approve(&fx, &api, &id).await {
        ApprovalActionOutcome::Failed { message } => {
            assert!(message.contains("could not check"), "{message}");
            assert!(message.contains("Approve again"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(api.requests().len(), 1, "no blind resend");
    assert_eq!(
        fx.store.slack_contact_send(&id).unwrap().unwrap().status,
        SlackSendStatus::Unverified
    );
    // The owner looked and approves again: that is the confirmation.
    assert!(matches!(
        approve(&fx, &api, &id).await,
        ApprovalActionOutcome::Approved
    ));
    assert_eq!(api.requests().len(), 2);
}

#[tokio::test]
async fn without_a_known_owner_account_nothing_is_sent_and_the_card_stays_pending() {
    let fx = fx();
    let api = Arc::new(FakeComposio::default()); // no owner user id
    let id = pending_reply(&fx, CHANNEL, "#general", None, "Hello");
    match approve(&fx, &api, &id).await {
        ApprovalActionOutcome::Failed { message } => {
            assert!(message.contains("as the app"), "{message}")
        }
        other => panic!("{other:?}"),
    }
    assert!(api.requests().is_empty());
    assert_eq!(row(&fx, &id).action.status, "pending");

    // No Composio connection for the workspace at all: same.
    let action = row(&fx, &id);
    assert!(matches!(
        approve_contact_message(&fx.store, None, &action, "slack").await,
        ApprovalActionOutcome::Failed { .. }
    ));
    assert_eq!(row(&fx, &id).action.status, "pending");
}

#[tokio::test]
async fn a_message_slack_attributes_to_an_app_is_flagged_not_silent() {
    let fx = fx();
    let api = FakeComposio::new();
    let id = pending_reply(&fx, CHANNEL, "#general", None, "Hello");
    api.script(Script::AsApp);
    assert!(matches!(
        approve(&fx, &api, &id).await,
        ApprovalActionOutcome::Approved
    ));
    let a = row(&fx, &id);
    assert_eq!(a.action.status, "sent");
    let note = a.action.error_message.unwrap_or_default();
    assert!(note.contains("app"), "the owner is told: {note}");
    assert_eq!(
        fx.store
            .slack_contact_send(&id)
            .unwrap()
            .unwrap()
            .observed_bot_id
            .as_deref(),
        Some("B0000000Z")
    );
}

#[tokio::test]
async fn model_markdown_is_converted_and_cannot_ping_the_channel() {
    let fx = fx();
    let api = FakeComposio::new();
    let id = pending_reply(
        &fx,
        CHANNEL,
        "#general",
        None,
        "**Yes** — see [the doc](https://example.com/doc) @channel <!here> <@U00000001>",
    );
    assert!(matches!(
        approve(&fx, &api, &id).await,
        ApprovalActionOutcome::Approved
    ));
    let text = &api.landed()[0].text;
    assert!(text.starts_with("*Yes*"), "{text}");
    assert!(text.contains("<https://example.com/doc|the doc>"), "{text}");
    assert!(!text.contains("<!here>"), "{text}");
    assert!(!text.contains("<@U00000001>"), "{text}");
    assert!(
        !text.contains("@channel"),
        "a word joiner breaks it: {text}"
    );
}

#[tokio::test]
async fn a_rejected_action_is_never_sent_by_a_later_approval() {
    let fx = fx();
    let api = FakeComposio::new();
    let id = pending_reply(&fx, CHANNEL, "#general", None, "No.");
    assert!(fx
        .store
        .try_resolve_action(&id, ActionStatus::Rejected, "slack", Some("skipped"))
        .unwrap());
    assert!(matches!(
        approve(&fx, &api, &id).await,
        ApprovalActionOutcome::AlreadyResolved { .. }
    ));
    // An error that did not come from a send (no ledger row) is not a retry.
    let other = pending_reply(&fx, "D00000078", "DM", None, "x");
    fx.store
        .update_action_status(
            &other,
            ActionStatus::Error,
            None,
            Some("post_approval: boom"),
        )
        .unwrap();
    assert!(matches!(
        approve(&fx, &api, &other).await,
        ApprovalActionOutcome::AlreadyResolved { .. }
    ));
    assert!(api.requests().is_empty());
}

// ---------------------------------------------------------------------------
// Compose
// ---------------------------------------------------------------------------

#[test]
fn a_person_resolves_through_the_wiki_identity_layer() {
    let fx = fx();
    match resolve_recipient(&fx.store, Some(&fx.wiki), TEAM, "alice") {
        Resolution::One(Recipient::Person {
            slug,
            name,
            user_id,
        }) => {
            assert_eq!(slug, "alice-example");
            assert_eq!(name, "Alice Example");
            assert_eq!(user_id, "U0000000A");
        }
        other => panic!("{other:?}"),
    }
    // Full name, slug and Slack id reach the same person.
    for q in [
        "Alice Example",
        "alice-example",
        "<@U0000000A>",
        "@U0000000A",
    ] {
        assert!(
            matches!(
                resolve_recipient(&fx.store, Some(&fx.wiki), TEAM, q),
                Resolution::One(Recipient::Person { ref user_id, .. }) if user_id == "U0000000A"
            ),
            "{q}"
        );
    }
}

#[test]
fn an_ambiguous_name_prompts_instead_of_picking_one() {
    let fx = fx();
    match resolve_recipient(&fx.store, Some(&fx.wiki), TEAM, "Alex") {
        Resolution::Ambiguous { candidates, .. } => {
            assert_eq!(candidates.len(), 2, "{candidates:?}");
            assert!(candidates.iter().any(|c| c.contains("Alex One")));
            assert!(candidates.iter().any(|c| c.contains("Alex Two")));
        }
        other => panic!("{other:?}"),
    }
    match compose(&fx.store, Some(&fx.wiki), TEAM, "Alex", "hi", false) {
        ComposeOutcome::Ambiguous { .. } => {}
        other => panic!("{other:?}"),
    }
    assert!(
        fx.store.oldest_pending_actions(10).unwrap().is_empty(),
        "nothing stored"
    );
}

#[test]
fn unknown_recipients_fail_closed() {
    let fx = fx();
    for q in ["Zed Nobody", "#random", "U0000000Q", ""] {
        assert!(
            matches!(
                resolve_recipient(&fx.store, Some(&fx.wiki), TEAM, q),
                Resolution::Unknown { .. }
            ),
            "{q}"
        );
    }
    match resolve_recipient(&fx.store, Some(&fx.wiki), TEAM, "Bob") {
        Resolution::Unknown { reason, .. } => assert!(reason.contains("Slack"), "{reason}"),
        other => panic!("{other:?}"),
    }
    // No wiki configured: people cannot be resolved at all.
    assert!(matches!(
        resolve_recipient(&fx.store, None, TEAM, "alice"),
        Resolution::Unknown { .. }
    ));
    assert!(fx.store.oldest_pending_actions(10).unwrap().is_empty());
}

#[test]
fn a_channel_resolves_through_the_workspace_subscriptions() {
    let fx = fx();
    match resolve_recipient(&fx.store, Some(&fx.wiki), TEAM, "#General") {
        Resolution::One(Recipient::Conversation {
            channel_id,
            label,
            kind,
        }) => {
            assert_eq!(channel_id, CHANNEL);
            assert_eq!(label, "#general");
            assert_eq!(kind, SlackConversationKind::Channel);
        }
        other => panic!("{other:?}"),
    }
    // Another workspace's channel is not this workspace's.
    assert!(matches!(
        resolve_recipient(&fx.store, Some(&fx.wiki), "T0000000X", "#general"),
        Resolution::Unknown { .. }
    ));
}

#[tokio::test]
async fn a_composed_message_is_carded_with_its_destination_then_sent_once_after_approval() {
    let fx = fx();
    let api = FakeComposio::new();
    let (id, email) = match compose(
        &fx.store,
        Some(&fx.wiki),
        TEAM,
        "Alice Example",
        "Can we move lunch to *Thursday*?",
        false,
    ) {
        ComposeOutcome::Card {
            action_id, email, ..
        } => (action_id, email),
        other => panic!("{other:?}"),
    };
    assert!(api.requests().is_empty(), "composing never sends");
    let a = row(&fx, &id);
    assert_eq!(a.action.status, "pending");
    assert_eq!(a.email.platform, "slack");
    assert_eq!(email.kind, "compose");
    let t = reply_target(&fx.store, &a.email).unwrap();
    assert_eq!(t.channel_id, "U0000000A");
    assert_eq!(t.kind, SlackConversationKind::User);
    assert_eq!(t.label.as_deref(), Some("Alice Example"));

    assert!(matches!(
        approve(&fx, &api, &id).await,
        ApprovalActionOutcome::Approved
    ));
    assert!(matches!(
        approve(&fx, &api, &id).await,
        ApprovalActionOutcome::AlreadyResolved { .. }
    ));
    let landed = api.landed();
    assert_eq!(landed.len(), 1);
    assert_eq!(
        landed[0].channel, "D0000000A",
        "Slack opened the owner's DM"
    );
    assert_eq!(landed[0].thread_ts, None);
    assert_eq!(landed[0].user, OWNER);
    let sent = fx.store.slack_contact_send(&id).unwrap().unwrap();
    assert_eq!(sent.channel_id, "U0000000A");
    assert_eq!(sent.remote_channel.as_deref(), Some("D0000000A"));
}

#[tokio::test]
async fn a_dry_run_compose_stores_nothing_and_sends_nothing() {
    let fx = fx();
    let api = FakeComposio::new();
    match compose(&fx.store, Some(&fx.wiki), TEAM, "#general", "Hi all", true) {
        ComposeOutcome::Preview {
            recipient: Recipient::Conversation { channel_id, .. },
        } => assert_eq!(channel_id, CHANNEL),
        other => panic!("{other:?}"),
    }
    assert!(fx.store.oldest_pending_actions(10).unwrap().is_empty());
    assert!(api.requests().is_empty());
}

#[tokio::test]
async fn a_composed_message_to_a_person_whose_send_timed_out_is_not_resent_blind() {
    // A send to a user id has no conversation id until Slack answers, so a
    // lost answer cannot be checked: the retry asks, then sends on the next
    // approval.
    let fx = fx();
    let api = FakeComposio::new();
    let id = match compose(&fx.store, Some(&fx.wiki), TEAM, "alice", "Hi", false) {
        ComposeOutcome::Card { action_id, .. } => action_id,
        other => panic!("{other:?}"),
    };
    api.script(Script::LandedButLost);
    assert!(matches!(
        approve(&fx, &api, &id).await,
        ApprovalActionOutcome::Failed { .. }
    ));
    match approve(&fx, &api, &id).await {
        ApprovalActionOutcome::Failed { message } => {
            assert!(message.contains("Alice Example"), "{message}")
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(api.requests().len(), 1);
    assert!(matches!(
        approve(&fx, &api, &id).await,
        ApprovalActionOutcome::Approved
    ));
    // Documented limitation: the owner confirmed, so it went out again.
    assert_eq!(api.landed().len(), 2);
}

#[test]
fn ingested_targets_thread_channel_replies_and_keep_dms_top_level() {
    let t = ingested_reply_target("C1:1.2", TEAM, "C00000009", "#general", "1.2", None);
    assert_eq!(
        (t.kind, t.thread_ts.as_deref()),
        (SlackConversationKind::Channel, Some("1.2"))
    );
    let t = ingested_reply_target("C1:1.3", TEAM, "C00000009", "#general", "1.3", Some("1.0"));
    assert_eq!(t.thread_ts.as_deref(), Some("1.0"));
    let t = ingested_reply_target("D1:1.2", TEAM, "D00000077", "DM with U1", "1.2", None);
    assert_eq!(
        (t.kind, t.thread_ts.as_deref()),
        (SlackConversationKind::Dm, None)
    );
    let t = ingested_reply_target(
        "D1:1.4",
        TEAM,
        "D00000077",
        "DM with U1",
        "1.4",
        Some("1.1"),
    );
    assert_eq!(
        t.thread_ts.as_deref(),
        Some("1.1"),
        "a DM thread reply stays in its thread"
    );
    let t = ingested_reply_target(
        "G1:1.2",
        TEAM,
        "G00000001",
        "group DM G00000001",
        "1.2",
        None,
    );
    assert_eq!(
        (t.kind, t.thread_ts.as_deref()),
        (SlackConversationKind::GroupDm, None)
    );
}

// ---------------------------------------------------------------------------
// #1291 — scheduled sends go through the same path
// ---------------------------------------------------------------------------

const ENGINE: &str = "scheduled-send-engine";

async fn send(
    fx: &Fx,
    api: &Arc<FakeComposio>,
    id: &str,
    claim: ContactClaim,
    source: &str,
) -> ApprovalActionOutcome {
    let action = row(fx, id);
    send_contact_message(
        &fx.store,
        Some(api.as_ref() as &dyn ContactSendApi),
        &action,
        source,
        claim,
    )
    .await
}

fn scheduled_reply(fx: &Fx, draft: &str, at_ms: i64) -> String {
    let id = pending_reply(fx, CHANNEL, "#general", Some(PARENT_TS), draft);
    assert!(fx.store.schedule_action(&id, at_ms, "slack").unwrap());
    id
}

#[tokio::test]
async fn a_scheduled_reply_fires_once_when_due_as_the_owner_in_its_thread_with_the_send_ledger() {
    let fx = fx();
    let api = FakeComposio::new();
    let id = scheduled_reply(&fx, "See you at nine.", 1_000_000);

    // Approve on an armed schedule is not a send.
    assert!(matches!(
        approve(&fx, &api, &id).await,
        ApprovalActionOutcome::AlreadyResolved { status, .. } if status == "scheduled"
    ));
    // Not due yet: nothing is claimed or posted.
    assert!(matches!(
        send(&fx, &api, &id, ContactClaim::Due { now_ms: 999_999 }, ENGINE).await,
        ApprovalActionOutcome::AlreadyResolved { status, .. } if status == "scheduled"
    ));
    assert!(api.requests().is_empty());
    assert_eq!(row(&fx, &id).action.status, "scheduled");

    // Due: one post, same destination and identity as an immediate send.
    let out = send(
        &fx,
        &api,
        &id,
        ContactClaim::Due { now_ms: 1_000_000 },
        ENGINE,
    )
    .await;
    assert!(matches!(out, ApprovalActionOutcome::Approved), "{out:?}");
    let landed = api.landed();
    assert_eq!(landed.len(), 1);
    assert_eq!(landed[0].channel, CHANNEL);
    assert_eq!(landed[0].thread_ts.as_deref(), Some(PARENT_TS));
    assert_eq!(landed[0].user, OWNER);
    assert_eq!(landed[0].text, "See you at nine.");
    assert_eq!(row(&fx, &id).action.status, "sent");
    assert_eq!(status_source(&fx, &id).as_deref(), Some(ENGINE));
    let ledger = fx.store.slack_contact_send(&id).unwrap().unwrap();
    assert_eq!(ledger.identity, SlackSendIdentity::OwnerUser);
    assert_eq!(ledger.sender_user_id, OWNER);
    assert_eq!(ledger.status, SlackSendStatus::Sent);

    // A second tick (or a restarted daemon) sends nothing.
    assert!(matches!(
        send(&fx, &api, &id, ContactClaim::Due { now_ms: 2_000_000 }, ENGINE).await,
        ApprovalActionOutcome::AlreadyResolved { status, .. } if status == "sent"
    ));
    assert_eq!(api.requests().len(), 1);
}

#[tokio::test]
async fn send_now_claims_a_scheduled_reply_whatever_its_time_and_the_timer_then_finds_nothing() {
    let fx = fx();
    let api = FakeComposio::new();
    let id = scheduled_reply(&fx, "Now, please.", 9_000_000_000_000);
    // Send now on a row that is not scheduled is refused.
    let pending = pending_reply(&fx, "C00000008", "#random", None, "Not scheduled.");
    assert!(matches!(
        send(&fx, &api, &pending, ContactClaim::SendNow, "slack").await,
        ApprovalActionOutcome::AlreadyResolved { status, .. } if status == "pending"
    ));
    assert!(api.requests().is_empty());

    let out = send(&fx, &api, &id, ContactClaim::SendNow, "slack").await;
    assert!(matches!(out, ApprovalActionOutcome::Approved), "{out:?}");
    assert_eq!(api.landed().len(), 1);
    assert_eq!(status_source(&fx, &id).as_deref(), Some("slack"));
    assert!(matches!(
        send(
            &fx,
            &api,
            &id,
            ContactClaim::Due {
                now_ms: 9_000_000_000_000
            },
            ENGINE
        )
        .await,
        ApprovalActionOutcome::AlreadyResolved { .. }
    ));
    assert_eq!(api.requests().len(), 1);
}

#[tokio::test]
async fn send_now_racing_the_scheduler_sends_once() {
    let fx = fx();
    let api = FakeComposio::new();
    let id = scheduled_reply(&fx, "Exactly once.", 1_000_000);
    let (a, b) = tokio::join!(
        send(&fx, &api, &id, ContactClaim::SendNow, "slack"),
        send(
            &fx,
            &api,
            &id,
            ContactClaim::Due { now_ms: 1_000_000 },
            ENGINE
        )
    );
    let sent = [&a, &b]
        .iter()
        .filter(|o| matches!(o, ApprovalActionOutcome::Approved))
        .count();
    assert_eq!(sent, 1, "{a:?} / {b:?}");
    assert_eq!(api.requests().len(), 1);
    assert_eq!(row(&fx, &id).action.status, "sent");
}

#[tokio::test]
async fn a_scheduled_send_slack_refused_is_retried_from_the_card_and_sent_once() {
    let fx = fx();
    let api = FakeComposio::new();
    let id = scheduled_reply(&fx, "Retry after refusal.", 1_000_000);
    api.script(Script::Refuse);
    assert!(matches!(
        send(
            &fx,
            &api,
            &id,
            ContactClaim::Due { now_ms: 1_000_000 },
            ENGINE
        )
        .await,
        ApprovalActionOutcome::Failed { .. }
    ));
    assert_eq!(row(&fx, &id).action.status, "error");
    assert!(api.landed().is_empty());
    // The errored card's Retry is an approval, exactly as for an immediate send.
    assert!(matches!(
        approve(&fx, &api, &id).await,
        ApprovalActionOutcome::Approved
    ));
    assert_eq!(api.landed().len(), 1);
    assert!(matches!(
        approve(&fx, &api, &id).await,
        ApprovalActionOutcome::AlreadyResolved { .. }
    ));
    assert_eq!(api.landed().len(), 1);
}

#[tokio::test]
async fn a_scheduled_send_that_cannot_start_leaves_the_schedule_armed_and_posts_nothing() {
    let fx = fx();
    let api = Arc::new(FakeComposio {
        owner: None,
        ..FakeComposio::default()
    });
    let id = scheduled_reply(&fx, "No identity.", 1_000_000);
    assert!(matches!(
        send(
            &fx,
            &api,
            &id,
            ContactClaim::Due { now_ms: 1_000_000 },
            ENGINE
        )
        .await,
        ApprovalActionOutcome::Failed { .. }
    ));
    assert!(api.requests().is_empty());
    assert_eq!(
        row(&fx, &id).action.status,
        "scheduled",
        "a refusal before the claim leaves the row as it was"
    );
}
