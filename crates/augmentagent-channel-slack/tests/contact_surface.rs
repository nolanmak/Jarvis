//! #1290 — composing and approving Slack contact messages from the owner's
//! Slack conversation, on the real approval surface (`SlackApprovals`) with
//! the real contact-send path behind the handler, a recording Web API (the
//! app bot: cards and answers to the owner) and a recording fake of the
//! Composio user connection (the only way anything reaches a contact).
//! Deterministic: every step is awaited directly; no clocks or sleeps.
//! Temporary store and wiki; ids and people are synthetic.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use augmentagent_approval_discord::{
    deciding_surface, ApprovalActionHandler, ApprovalActionOutcome, CardSurfaces,
};
use augmentagent_channel_slack::approvals::{SlackApprovalConfig, SlackApprovals};
use augmentagent_channel_slack::contact::{
    approve_contact_message, ingested_reply_target, ContactSendApi, ContactSendError,
    OutgoingContactMessage, PostedContactMessage,
};
use augmentagent_channel_slack::surface::SlackWorkspace;
use augmentagent_channel_slack::transport::web::{
    PostMessage, RecordedCall, RecordingSlackWebApi, SlackWebApi, UpdateMessage,
};
use augmentagent_store::{ActionStatus, Email, Store, SubscriptionMode};

const APP_TEAM: &str = "T00000001";
const TEAM: &str = "T00000009";
const OWNER: &str = "U00000009";
const OWNER_DM: &str = "D00000001";

#[derive(Default)]
struct FakeComposio {
    fail_next: Mutex<VecDeque<ContactSendError>>,
    posts: Mutex<Vec<OutgoingContactMessage>>,
}

#[async_trait]
impl ContactSendApi for FakeComposio {
    fn owner_user_id(&self) -> Option<String> {
        Some(OWNER.into())
    }
    async fn post_as_owner(
        &self,
        m: &OutgoingContactMessage,
    ) -> Result<PostedContactMessage, ContactSendError> {
        if let Some(e) = self.fail_next.lock().unwrap().pop_front() {
            return Err(e);
        }
        let mut posts = self.posts.lock().unwrap();
        posts.push(m.clone());
        Ok(PostedContactMessage {
            channel: m.channel.replace('U', "D"),
            ts: format!("1700000100.{:06}", posts.len()),
            user: Some(OWNER.into()),
            bot_id: None,
        })
    }
    async fn find_owner_message(
        &self,
        _: &str,
        _: Option<&str>,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<Option<String>, ContactSendError> {
        Ok(None)
    }
}

/// The daemon's handler for Slack rows: the real contact-send path.
struct ContactHandler {
    store: Arc<Store>,
    api: Arc<FakeComposio>,
    surfaces_seen: Mutex<Vec<Option<&'static str>>>,
}

#[async_trait]
impl ApprovalActionHandler for ContactHandler {
    async fn approve(&self, id: &str) -> ApprovalActionOutcome {
        self.surfaces_seen.lock().unwrap().push(deciding_surface());
        let Some(action) = self.store.get_action_with_email(id).unwrap() else {
            return ApprovalActionOutcome::NotFound;
        };
        let source = deciding_surface().unwrap_or("discord");
        approve_contact_message(
            &self.store,
            Some(self.api.as_ref() as &dyn ContactSendApi),
            &action,
            source,
        )
        .await
    }
    async fn revise(&self, _: &str, _: &str) -> ApprovalActionOutcome {
        ApprovalActionOutcome::Failed {
            message: "no reasoner here".into(),
        }
    }
    async fn skip(&self, id: &str) -> ApprovalActionOutcome {
        if self
            .store
            .try_resolve_action(id, ActionStatus::Rejected, "slack", Some("skipped"))
            .unwrap()
        {
            ApprovalActionOutcome::Skipped
        } else {
            ApprovalActionOutcome::AlreadyResolved {
                status: "resolved".into(),
                detail: None,
            }
        }
    }
    async fn is_resolved(&self, id: &str) -> bool {
        self.store
            .get_action_with_email(id)
            .unwrap()
            .is_some_and(|a| a.action.status != "pending")
    }
}

struct H {
    _dir: tempfile::TempDir,
    store: Arc<Store>,
    web: Arc<RecordingSlackWebApi>,
    api: Arc<FakeComposio>,
    handler: Arc<ContactHandler>,
    approvals: Arc<SlackApprovals>,
}

fn page(wiki: &std::path::Path, slug: &str, title: &str, ids: &str) {
    let dir = wiki.join("people");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{slug}.md")),
        format!("---\nkind: person\nidentities:\n{ids}---\n# {title}\n"),
    )
    .unwrap();
}

fn harness_with(wiki: Option<PathBuf>, dir: tempfile::TempDir, store: Arc<Store>) -> H {
    let web = Arc::new(RecordingSlackWebApi::default());
    let api = Arc::new(FakeComposio::default());
    let handler = Arc::new(ContactHandler {
        store: Arc::clone(&store),
        api: Arc::clone(&api),
        surfaces_seen: Mutex::new(Vec::new()),
    });
    let approvals = Arc::new(
        SlackApprovals::new(
            Arc::clone(&store),
            Arc::clone(&web) as Arc<dyn SlackWebApi>,
            SlackApprovalConfig {
                workspace: SlackWorkspace::new(APP_TEAM, None).unwrap(),
                channel: OWNER_DM.into(),
            },
            CardSurfaces::new(),
        )
        .with_wiki_root(wiki),
    );
    approvals.set_handler(handler.clone());
    H {
        _dir: dir,
        store,
        web,
        api,
        handler,
        approvals,
    }
}

fn harness() -> H {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("state dir ü");
    std::fs::create_dir_all(&root).unwrap();
    let store = Arc::new(Store::open(root.join("data.db")).unwrap());
    store
        .upsert_slack_workspace(TEAM, "Contacts Example", "entity-test", "conn-test", OWNER)
        .unwrap();
    store
        .upsert_subscription(
            "slack",
            "C00000009",
            "#general",
            SubscriptionMode::Priority,
            Some(TEAM),
        )
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
    harness_with(Some(wiki), dir, store)
}

impl H {
    fn posts(&self) -> Vec<PostMessage> {
        self.web
            .calls()
            .into_iter()
            .filter_map(|c| match c {
                RecordedCall::PostMessage(p) => Some(p),
                _ => None,
            })
            .collect()
    }
    fn updates(&self) -> Vec<UpdateMessage> {
        self.web
            .calls()
            .into_iter()
            .filter_map(|c| match c {
                RecordedCall::UpdateMessage(u) => Some(u),
                _ => None,
            })
            .collect()
    }
    fn cards(&self) -> Vec<String> {
        self.posts()
            .into_iter()
            .filter(|p| {
                p.blocks
                    .as_ref()
                    .is_some_and(|b| b.to_string().contains("aa_approve"))
            })
            .map(|p| {
                assert_eq!(p.channel, OWNER_DM, "cards only ever go to the owner");
                p.blocks.unwrap().to_string()
            })
            .collect()
    }
    fn pending(&self) -> Vec<String> {
        self.store
            .oldest_pending_actions(50)
            .unwrap()
            .into_iter()
            .map(|r| r.0)
            .collect()
    }
    fn contact_posts(&self) -> Vec<OutgoingContactMessage> {
        self.api.posts.lock().unwrap().clone()
    }
}

#[tokio::test]
async fn a_compose_command_posts_a_card_showing_the_destination_and_sender_and_sends_nothing() {
    let h = harness();
    let reply = h
        .approvals
        .handle_command("compose Alice Example: Can we move lunch to Thursday?")
        .await
        .expect("a compose command is an approval command");
    assert!(reply.contains("Alice Example"), "{reply}");
    assert!(reply.contains("approve"), "{reply}");
    let cards = h.cards();
    assert_eq!(cards.len(), 1);
    let card = &cards[0];
    assert!(card.contains("*To*"), "{card}");
    assert!(card.contains("DM with Alice Example"), "{card}");
    assert!(card.contains("Sends as"), "{card}");
    assert!(card.contains(OWNER), "{card}");
    assert!(card.contains("never the app bot"), "{card}");
    assert!(card.contains("Can we move lunch to Thursday?"), "{card}");
    assert!(h.contact_posts().is_empty(), "composing never sends");
    assert_eq!(h.pending().len(), 1);
}

#[tokio::test]
async fn approving_the_composed_card_sends_once_as_the_owner_and_records_slack() {
    let h = harness();
    h.approvals
        .handle_command("compose #general: Standup moved to 10")
        .await
        .unwrap();
    let id = h.pending().remove(0);
    let r = &id[..8];
    let first = h
        .approvals
        .handle_command(&format!("approve {r}"))
        .await
        .unwrap();
    assert!(first.contains("Approved"), "{first}");
    let again = h
        .approvals
        .handle_command(&format!("approve {r}"))
        .await
        .unwrap();
    assert!(again.contains("Already sent"), "{again}");
    let sent = h.contact_posts();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].channel, "C00000009");
    assert_eq!(sent[0].text, "Standup moved to 10");
    assert_eq!(
        h.store.action_status_source(&id).unwrap().as_deref(),
        Some("slack"),
        "a decision taken on Slack is recorded as Slack's"
    );
    assert_eq!(
        *h.handler.surfaces_seen.lock().unwrap(),
        vec![Some("slack"), Some("slack")]
    );
    assert!(
        h.updates().iter().any(|u| u.text.contains("Sent")),
        "the card was redrawn in place"
    );
}

#[tokio::test]
async fn an_ambiguous_name_prompts_and_an_unknown_name_fails_closed() {
    let h = harness();
    let ask = h
        .approvals
        .handle_command("compose Alex: hi")
        .await
        .unwrap();
    assert!(
        ask.contains("Alex One") && ask.contains("Alex Two"),
        "{ask}"
    );
    assert!(ask.contains("Which"), "{ask}");
    let no = h
        .approvals
        .handle_command("compose Zed Nobody: hi")
        .await
        .unwrap();
    assert!(no.contains("Not sent"), "{no}");
    let usage = h.approvals.handle_command("compose Alice").await.unwrap();
    assert!(
        usage.contains("compose <person or #channel>: <message>"),
        "{usage}"
    );
    assert!(h.cards().is_empty());
    assert!(h.pending().is_empty());
    assert!(h.contact_posts().is_empty());
}

#[tokio::test]
async fn a_failed_send_leaves_a_retry_on_the_card_and_the_retry_sends_once() {
    let h = harness();
    h.approvals
        .handle_command("compose alice: Hello")
        .await
        .unwrap();
    let id = h.pending().remove(0);
    h.api
        .fail_next
        .lock()
        .unwrap()
        .push_back(ContactSendError::Rejected("ratelimited".into()));
    let r = &id[..8];
    let failed = h
        .approvals
        .handle_command(&format!("approve {r}"))
        .await
        .unwrap();
    assert!(failed.contains("nothing was sent"), "{failed}");
    let redrawn = h.updates().last().cloned().expect("the card was redrawn");
    let blocks = redrawn.blocks.unwrap().to_string();
    assert!(blocks.contains("Not sent"), "{blocks}");
    assert!(blocks.contains("Retry send"), "{blocks}");
    assert!(blocks.contains(&format!("approve {r}")), "{blocks}");
    assert!(h.contact_posts().is_empty());

    let ok = h
        .approvals
        .handle_command(&format!("approve {r}"))
        .await
        .unwrap();
    assert!(ok.contains("Approved"), "{ok}");
    assert_eq!(h.contact_posts().len(), 1);
    assert_eq!(h.contact_posts()[0].channel, "U0000000A");
    let again = h
        .approvals
        .handle_command(&format!("approve {r}"))
        .await
        .unwrap();
    assert!(again.contains("Already sent"), "{again}");
    assert_eq!(h.contact_posts().len(), 1);
}

#[tokio::test]
async fn a_reply_card_names_the_conversation_thread_and_sender() {
    let h = harness();
    let email = Email {
        message_id: "C00000009:1700000000.000200".into(),
        thread_id: Some("C00000009".into()),
        from: "Contact Example <slack:U00000077>".into(),
        to: String::new(),
        cc: String::new(),
        attachments: Vec::new(),
        subject: String::new(),
        body: "Can you review?".into(),
        date: String::new(),
        account_entity_id: Some(format!("slack:team:{TEAM}")),
        platform: "slack".into(),
        kind: "dm".into(),
    };
    h.store.upsert_email(&email).unwrap();
    h.store
        .record_slack_send_target(&ingested_reply_target(
            &email.message_id,
            TEAM,
            "C00000009",
            "#general",
            "1700000000.000200",
            Some("1700000000.000100"),
        ))
        .unwrap();
    let id = h
        .store
        .log_action(
            &email.message_id,
            email.thread_id.as_deref(),
            &email.from,
            "",
            Some(&email.body),
            Some("Looking now."),
            ActionStatus::Pending,
        )
        .unwrap();
    use augmentagent_approval_discord::ApprovalBroker;
    h.approvals
        .post_approval(&id, &email, "Looking now.")
        .await
        .unwrap();
    let card = h.cards().remove(0);
    assert!(card.contains("*From*"), "{card}");
    assert!(
        card.contains("#general · in thread 1700000000.000100"),
        "{card}"
    );
    assert!(card.contains("Sends as"), "{card}");
    h.approvals
        .handle_command(&format!("approve {}", &id[..8]))
        .await
        .unwrap();
    let sent = h.contact_posts();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].thread_ts.as_deref(), Some("1700000000.000100"));
}

#[tokio::test]
async fn without_a_wiki_a_person_cannot_be_composed_to() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("data.db")).unwrap());
    store
        .upsert_slack_workspace(TEAM, "Contacts Example", "entity-test", "conn-test", OWNER)
        .unwrap();
    let h = harness_with(None, dir, store);
    let no = h
        .approvals
        .handle_command("compose alice: hi")
        .await
        .unwrap();
    assert!(no.contains("Not sent"), "{no}");
    assert!(h.pending().is_empty());
}

/// Nothing the agent writes in the owner's conversation — an answer, a tool
/// result pasted into it, or text that looks like a command — is a contact
/// send. Only a card the owner approves sends, and it sends the card's
/// draft, not the agent's words.
#[tokio::test]
async fn control_conversation_text_is_never_a_contact_send() {
    let h = harness();
    // What an agent answer or tool output could contain.
    for text in [
        "Done — I sent “lunch moved” to Alice Example.",
        "send Alice Example: lunch moved",
        "tool output: {\"channel\": \"U0000000A\", \"text\": \"lunch moved\"}",
        "@channel <!here> message #general: hi everyone",
    ] {
        assert_eq!(
            h.approvals.handle_command(text).await,
            None,
            "not an approval command: {text}"
        );
    }
    assert!(h.contact_posts().is_empty());
    assert!(h.pending().is_empty());

    // The agent's compose tool only proposes; the draft is what it proposed.
    h.approvals
        .handle_command("compose alice: The draft the owner will see")
        .await
        .unwrap();
    assert!(h.contact_posts().is_empty());
    // A reference that names no action sends nothing.
    let none = h
        .approvals
        .handle_command("approve 00000000")
        .await
        .unwrap();
    assert!(none.contains("No approval matches"), "{none}");
    assert!(h.contact_posts().is_empty());

    let id = h.pending().remove(0);
    h.approvals
        .handle_command(&format!("approve {}", &id[..8]))
        .await
        .unwrap();
    let sent = h.contact_posts();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].text, "The draft the owner will see");
    // Everything the app bot posted went to the owner, never a contact.
    assert!(h.posts().iter().all(|p| p.channel == OWNER_DM));
}
