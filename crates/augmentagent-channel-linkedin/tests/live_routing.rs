use async_trait::async_trait;
use augmentagent_approval_discord::{ApprovalBroker, ApprovalError};
use augmentagent_channel_core::{Reasoner, ReasonerOpts};
use augmentagent_channel_linkedin::{
    archive::ArchiveReader,
    archive_live,
    posting::{PostDraft, ShareUrn},
    types::{Dm, FeedPost},
    LinkedInApi, LinkedInChannel, LinkedInChannelConfig, LinkedInError,
};
use augmentagent_store::{Email, Store};
use serde_json::json;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use wiremock::{matchers::method, Mock, MockServer, Request, ResponseTemplate};
mod support;
use support::{auth, OWNER};
struct Api {
    base: String,
    sends: AtomicUsize,
}
#[async_trait]
impl LinkedInApi for Api {
    fn archive_reader(&self) -> Result<Option<ArchiveReader>, LinkedInError> {
        Ok(Some(
            ArchiveReader::for_test(auth(), self.base.clone()).unwrap(),
        ))
    }
    async fn fetch_recent_dms(&self) -> Result<Vec<Dm>, LinkedInError> {
        panic!("must not fall back to snippets")
    }
    async fn send_message(&self, _: &str, _: &str) -> Result<String, LinkedInError> {
        self.sends.fetch_add(1, Ordering::SeqCst);
        Ok("sent".into())
    }
    async fn fetch_feed_posts_by_author(&self, _: &str) -> Result<Vec<FeedPost>, LinkedInError> {
        panic!("not a mailbox read")
    }
    async fn post_comment(&self, _: &str, _: &str) -> Result<String, LinkedInError> {
        panic!("no writes")
    }
    async fn react(&self, _: &str, _: &str) -> Result<(), LinkedInError> {
        panic!("no writes")
    }
    async fn create_share(&self, _: PostDraft<'_>) -> Result<ShareUrn, LinkedInError> {
        panic!("no writes")
    }
}
struct SpyReasoner {
    calls: AtomicUsize,
    skip_only: bool,
}
#[async_trait]
impl Reasoner for SpyReasoner {
    async fn call(&self, opts: &ReasonerOpts, prompt: &str) -> anyhow::Result<String> {
        for i in 0..3 {
            assert!(
                prompt.contains(&format!("burst message {i}")),
                "missing burst context {i}"
            );
        }
        if opts.system_prompt.contains("You select evidence") {
            return Ok(r#"{"action":"ready","facts":[],"gaps":[]}"#.into());
        }
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if self.skip_only {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            return Ok(r#"{"decision":"skip","reason":"fixture"}"#.into());
        }
        Ok(if n == 0 {
            r#"{"decision":"reply","reason":"direct question"}"#
        } else {
            "Thanks, I'll take a look."
        }
        .into())
    }
}
#[derive(Default)]
struct Broker {
    cards: AtomicUsize,
    notices: AtomicUsize,
}
#[async_trait]
impl ApprovalBroker for Broker {
    async fn post_approval(&self, _: &str, _: &Email, _: &str) -> Result<(), ApprovalError> {
        self.cards.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn post_flag_notice(&self, _: &Email, _: &str) -> Result<(), ApprovalError> {
        self.notices.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
async fn exercise(group: bool, owner_latest: bool, historical: bool, concurrent: bool) {
    std::env::set_var("AUGMENTAGENT_GH_DISABLE", "1");
    let server = MockServer::start().await;
    Mock::given(method("GET")).respond_with(move|r:&Request|{
  let query=r.url.query().unwrap_or("");
  if query.contains("messengerMailboxCounts") {return ResponseTemplate::new(200).set_body_json(json!({"data":{"messengerMailboxCountsByMailbox":{"elements":[{"category":"PRIMARY_INBOX"}]}}}));}
  let conv=json!({"entityUrn":"thread:live","title":"Live","groupChat":group,"lastActivityAt":2002,"conversationParticipants":[{"hostIdentityUrn":OWNER},{"hostIdentityUrn":"peer"}]});
  if query.contains("messengerConversationsId") {return ResponseTemplate::new(200).set_body_json(json!({"data":{"messengerConversationsById":conv}}));}
  if r.url.path().ends_with("graphql") {return ResponseTemplate::new(200).set_body_json(json!({"data":{"messengerConversationsByCategoryQuery":{"elements":[conv],"metadata":{"nextCursor":null}}}}));}
  ResponseTemplate::new(200).set_body_json(json!({"elements":(0..3).map(|i|json!({"backendUrn":format!("msg:{i}"),"senderUrn":if owner_latest&&i==2{OWNER}else{"peer"},"deliveredAt":2000+i,"body":{"text":format!("burst message {i}")}})).collect::<Vec<_>>(),"metadata":{"nextCursor":null}}))
 }).mount(&server).await;
    let file = tempfile::NamedTempFile::new().unwrap();
    let store = Arc::new(Store::open(file.path()).unwrap());
    archive_live::initialize(&store, OWNER, if historical { 3000 } else { 1000 }).unwrap();
    let api = Arc::new(Api {
        base: server.uri(),
        sends: AtomicUsize::new(0),
    });
    let reasoner = Arc::new(SpyReasoner {
        calls: AtomicUsize::new(0),
        skip_only: concurrent,
    });
    let broker = Arc::new(Broker::default());
    let channel = LinkedInChannel::new(
        store.clone(),
        api.clone(),
        reasoner.clone(),
        broker.clone(),
        OWNER.into(),
        LinkedInChannelConfig {
            dry_run: false,
            ..Default::default()
        },
    );
    if concurrent {
        let reader = api.archive_reader().unwrap().unwrap();
        let email = archive_live::poll(
            &reader,
            &store,
            100,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap()
        .remove(0);
        let (a, b) = tokio::join!(
            channel.process_email(email.clone()),
            channel.process_email(email)
        );
        a.unwrap();
        b.unwrap();
        assert_eq!(
            reasoner.calls.load(Ordering::SeqCst),
            1,
            "concurrent dispatch must claim the same live candidate once"
        );
        assert_eq!(
            store
                .with_conn(
                    |c| c.query_row("SELECT COUNT(*) FROM actions", [], |r| r.get::<_, i64>(0))
                )
                .unwrap(),
            1
        );
        assert_eq!(api.sends.load(Ordering::SeqCst), 0);
        return;
    }
    let first = channel.poll_once().await.unwrap();
    if group || owner_latest || historical {
        assert_eq!(reasoner.calls.load(Ordering::SeqCst), 0);
        assert_eq!(broker.cards.load(Ordering::SeqCst), 0);
        assert_eq!(broker.notices.load(Ordering::SeqCst), 0);
    } else {
        assert_eq!(first.awaiting_approval, 1);
        assert_eq!(broker.cards.load(Ordering::SeqCst), 1);
        let receipts: i64 = store.with_conn(|c| c.query_row(
            "SELECT COUNT(*) FROM draft_context_receipts WHERE action_id IS NOT NULL",
            [], |r| r.get(0),
        )).unwrap();
        assert_eq!(receipts, 1, "the live card retains its shared context receipt");
    }
    let before = reasoner.calls.load(Ordering::SeqCst);
    channel.poll_once().await.unwrap();
    assert_eq!(reasoner.calls.load(Ordering::SeqCst), before);
    assert_eq!(api.sends.load(Ordering::SeqCst), 0);
    assert_eq!(
        store
            .with_conn(
                |c| c.query_row("SELECT COUNT(*) FROM linkedin_archive_messages", [], |r| {
                    r.get::<_, i64>(0)
                })
            )
            .unwrap(),
        3
    );
    assert!(server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|r| r.method.as_str() == "GET"));
}
#[tokio::test]
async fn direct_burst_reaches_one_card_with_full_context_and_no_send() {
    exercise(false, false, false, false).await;
}
#[tokio::test]
async fn history_group_and_owner_messages_stay_archive_only() {
    for (group, owner, historical) in [
        (true, false, false),
        (false, true, false),
        (false, false, true),
    ] {
        exercise(group, owner, historical, false).await;
    }
}

#[tokio::test]
async fn concurrent_live_dispatch_does_not_retriage_the_same_burst() {
    exercise(false, false, false, true).await;
}

#[tokio::test]
async fn production_poll_archives_full_corpus_and_replays_without_automation() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(support::respond)
        .mount(&server)
        .await;
    let file = tempfile::NamedTempFile::new().unwrap();
    for _ in 0..2 {
        let store = Arc::new(Store::open(file.path()).unwrap());
        archive_live::initialize(&store, OWNER, 3000).unwrap();
        let api = Arc::new(Api {
            base: server.uri(),
            sends: AtomicUsize::new(0),
        });
        let reasoner = Arc::new(SpyReasoner {
            calls: AtomicUsize::new(0),
            skip_only: false,
        });
        let broker = Arc::new(Broker::default());
        let channel = LinkedInChannel::new(
            store.clone(),
            api.clone(),
            reasoner.clone(),
            broker.clone(),
            OWNER.into(),
            LinkedInChannelConfig {
                dry_run: false,
                ..Default::default()
            },
        );
        channel.poll_once().await.unwrap();
        let coverage =
            augmentagent_channel_linkedin::archive_store::coverage(&store, OWNER, "PRIMARY_INBOX")
                .unwrap();
        assert_eq!(
            (
                coverage.conversations,
                coverage.messages,
                coverage.owner_messages,
                coverage.group_conversations
            ),
            (45, 153, 51, 3)
        );
        assert!(coverage.complete);
        assert_eq!(reasoner.calls.load(Ordering::SeqCst), 0);
        assert_eq!(broker.cards.load(Ordering::SeqCst), 0);
        assert_eq!(broker.notices.load(Ordering::SeqCst), 0);
        assert_eq!(api.sends.load(Ordering::SeqCst), 0);
        assert_eq!(
            store
                .with_conn(
                    |c| c.query_row("SELECT COUNT(*) FROM actions", [], |r| r.get::<_, i64>(0))
                )
                .unwrap(),
            0
        );
    }
    assert_eq!(server.received_requests().await.unwrap().len(), 59);
}
