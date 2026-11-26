//! A dead session reaches the owner once, backs off, and recovers (#2255).
use async_trait::async_trait;
use augmentagent_approval_discord::{ApprovalBroker, ApprovalError};
use augmentagent_channel_core::InboundSource;
use augmentagent_channel_linkedin::{
    archive::ArchiveReader,
    archive_store as db, archive_sync,
    session_guard::{self, Notice},
    LinkedInInbound, VoyagerClient,
};
use augmentagent_store::{Email, Store};
use std::sync::{
    atomic::{AtomicBool, AtomicI64, Ordering},
    Arc, Mutex,
};
use tokio_util::sync::CancellationToken;
use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};
mod support;
use support::*;

const MINUTE: i64 = 60_000;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;

#[derive(Default)]
struct Broker {
    notices: Mutex<Vec<String>>,
    fail: AtomicBool,
}
#[async_trait]
impl ApprovalBroker for Broker {
    async fn post_approval(&self, _: &str, _: &Email, _: &str) -> Result<(), ApprovalError> {
        panic!("a session notice is never an approval card")
    }
    async fn post_flag_notice(&self, email: &Email, reason: &str) -> Result<(), ApprovalError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(ApprovalError::Discord("offline".into()));
        }
        self.notices
            .lock()
            .unwrap()
            .push(format!("{}\n{reason}", email.subject));
        Ok(())
    }
}
impl Broker {
    fn count(&self) -> usize {
        self.notices.lock().unwrap().len()
    }
    fn last(&self) -> String {
        self.notices.lock().unwrap().last().cloned().unwrap()
    }
}

/// Redirects every read until `healthy` is set, like a logged-out session.
async fn server(healthy: Arc<AtomicBool>) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(move |r: &wiremock::Request| {
            if healthy.load(Ordering::SeqCst) {
                respond(r)
            } else {
                ResponseTemplate::new(302).insert_header("Location", "/login")
            }
        })
        .mount(&server)
        .await;
    server
}

async fn requests(server: &MockServer) -> usize {
    server.received_requests().await.unwrap().len()
}

#[tokio::test]
async fn dead_session_posts_one_notice_repeats_daily_and_reports_recovery_once() {
    let healthy = Arc::new(AtomicBool::new(false));
    let server = server(healthy.clone()).await;
    let reader = ArchiveReader::for_test(auth(), server.uri()).unwrap();
    let file = tempfile::NamedTempFile::new().unwrap();
    let store = Store::open(file.path()).unwrap();
    let broker = Broker::default();
    let clock = AtomicI64::new(1_000_000);
    let now = || clock.load(Ordering::SeqCst);
    let cancel = CancellationToken::new();

    let report =
        archive_sync::sync_with_clock(&reader, &store, "PRIMARY_INBOX", 100, &cancel, &now)
            .await
            .unwrap();
    assert!(report.account_deferred);
    let first = session_guard::notify(&store, &broker, OWNER, now())
        .await
        .unwrap();
    assert!(matches!(first, Some(Notice::Dead { .. })), "{first:?}");
    assert_eq!(broker.count(), 1);
    let text = broker.last();
    assert!(text.contains(OWNER), "{text}");
    assert!(text.contains("augmentagent linkedin login"), "{text}");
    assert!(text.contains("not being read"), "{text}");
    let dead = session_guard::dead_sessions(&store).unwrap();
    assert_eq!(dead.len(), 1);
    assert_eq!(
        (dead[0].account.as_str(), dead[0].since_ms),
        (OWNER, 1_000_000)
    );
    assert!(session_guard::digest_line(&store)
        .unwrap()
        .is_some_and(|line| line.contains("LinkedIn") && !line.contains('\n')));

    // Polls keep failing for the rest of the day; the owner hears nothing more,
    // including after a daemon restart.
    drop(store);
    let store = Store::open(file.path()).unwrap();
    for hour in [4, 8, 12, 16, 20] {
        clock.store(1_000_000 + hour * HOUR, Ordering::SeqCst);
        archive_sync::sync_with_clock(&reader, &store, "PRIMARY_INBOX", 100, &cancel, &now)
            .await
            .unwrap();
        assert_eq!(
            session_guard::notify(&store, &broker, OWNER, now())
                .await
                .unwrap(),
            None
        );
    }
    assert_eq!(broker.count(), 1);
    assert!(requests(&server).await > 1, "the session was retried");

    clock.store(1_000_000 + DAY, Ordering::SeqCst);
    let reminder = session_guard::notify(&store, &broker, OWNER, now())
        .await
        .unwrap();
    assert!(matches!(reminder, Some(Notice::Dead { .. })));
    assert_eq!(broker.count(), 2);
    assert_eq!(
        session_guard::dead_sessions(&store).unwrap()[0].since_ms,
        1_000_000,
        "a reminder keeps the original outage start"
    );

    // A fresh login with a working session: the next read recovers.
    healthy.store(true, Ordering::SeqCst);
    assert!(session_guard::clear_pause_after_login(&store, OWNER).unwrap());
    assert_eq!(db::account_retry_at(&store, OWNER).unwrap(), 0);
    assert_eq!(
        session_guard::dead_sessions(&store).unwrap().len(),
        1,
        "only a successful read clears the finding"
    );
    let report = archive_sync::sync_with_clock(&reader, &store, "PRIMARY_INBOX", 1, &cancel, &now)
        .await
        .unwrap();
    assert_eq!(report.pages, 1);
    assert!(session_guard::dead_sessions(&store).unwrap().is_empty());
    assert_eq!(session_guard::digest_line(&store).unwrap(), None);
    assert_eq!(
        session_guard::notify(&store, &broker, OWNER, now())
            .await
            .unwrap(),
        Some(Notice::Recovered)
    );
    assert_eq!(broker.count(), 3);
    assert!(broker.last().contains("reading again"), "{}", broker.last());
    clock.store(1_000_000 + 3 * DAY, Ordering::SeqCst);
    assert_eq!(
        session_guard::notify(&store, &broker, OWNER, now())
            .await
            .unwrap(),
        None
    );
    assert_eq!(broker.count(), 3);
}

#[tokio::test]
async fn session_failures_back_off_from_a_minute_to_the_cap_until_login() {
    let healthy = Arc::new(AtomicBool::new(false));
    let server = server(healthy.clone()).await;
    let reader = ArchiveReader::for_test(auth(), server.uri()).unwrap();
    let store = Store::open(":memory:").unwrap();
    let clock = AtomicI64::new(0);
    let now = || clock.load(Ordering::SeqCst);
    let cancel = CancellationToken::new();
    let mut delays = Vec::new();
    for attempt in 1..=12 {
        archive_sync::sync_with_clock(&reader, &store, "PRIMARY_INBOX", 100, &cancel, &now)
            .await
            .unwrap();
        assert_eq!(requests(&server).await, attempt);
        let retry = db::account_retry_at(&store, OWNER).unwrap();
        delays.push(retry - now());
        if attempt >= 2 {
            // The old fixed backoff would retry here.
            clock.fetch_add(MINUTE, Ordering::SeqCst);
            let early =
                archive_sync::sync_with_clock(&reader, &store, "PRIMARY_INBOX", 100, &cancel, &now)
                    .await
                    .unwrap();
            assert_eq!((early.pages, early.stopped.as_str()), (0, "retry_deferred"));
            assert_eq!(requests(&server).await, attempt);
        }
        clock.store(retry, Ordering::SeqCst);
    }
    assert_eq!(
        &delays[..5],
        &[MINUTE, 2 * MINUTE, 4 * MINUTE, 8 * MINUTE, 16 * MINUTE]
    );
    assert!(delays.windows(2).all(|pair| pair[0] <= pair[1]));
    assert_eq!(delays[11], session_guard::MAX_BACKOFF_MS);
    assert_eq!(delays[10], session_guard::MAX_BACKOFF_MS);
    assert!(session_guard::MAX_BACKOFF_MS >= 2 * HOUR);
    // The scope checkpoint carries the same pause as the account.
    assert_eq!(
        db::checkpoint(&store, OWNER, "PRIMARY_INBOX")
            .unwrap()
            .retry_at_ms,
        db::account_retry_at(&store, OWNER).unwrap()
    );

    // Mid-pause, a fresh login makes the very next read go out and restarts
    // the backoff from one minute.
    clock.fetch_sub(HOUR, Ordering::SeqCst);
    assert!(session_guard::clear_pause_after_login(&store, OWNER).unwrap());
    archive_sync::sync_with_clock(&reader, &store, "PRIMARY_INBOX", 100, &cancel, &now)
        .await
        .unwrap();
    assert_eq!(requests(&server).await, 13);
    assert_eq!(db::account_retry_at(&store, OWNER).unwrap() - now(), MINUTE);
}

#[tokio::test]
async fn throttling_is_not_a_dead_session_and_login_does_not_lift_it() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "600"))
        .mount(&server)
        .await;
    let reader = ArchiveReader::for_test(auth(), server.uri()).unwrap();
    let store = Store::open(":memory:").unwrap();
    let clock = AtomicI64::new(1000);
    let now = || clock.load(Ordering::SeqCst);
    archive_sync::sync_with_clock(
        &reader,
        &store,
        "PRIMARY_INBOX",
        100,
        &CancellationToken::new(),
        &now,
    )
    .await
    .unwrap();
    assert_eq!(db::account_retry_at(&store, OWNER).unwrap(), 601_000);
    assert!(session_guard::dead_sessions(&store).unwrap().is_empty());
    let broker = Broker::default();
    assert_eq!(
        session_guard::notify(&store, &broker, OWNER, now())
            .await
            .unwrap(),
        None
    );
    assert!(!session_guard::clear_pause_after_login(&store, OWNER).unwrap());
    assert_eq!(db::account_retry_at(&store, OWNER).unwrap(), 601_000);
}

#[tokio::test]
async fn an_undelivered_notice_is_posted_on_the_next_poll() {
    let store = Store::open(":memory:").unwrap();
    session_guard::record_failure(&store, OWNER, "LinkedIn session rejected", 5000).unwrap();
    let broker = Broker::default();
    broker.fail.store(true, Ordering::SeqCst);
    assert!(session_guard::notify(&store, &broker, OWNER, 5000)
        .await
        .is_err());
    broker.fail.store(false, Ordering::SeqCst);
    assert!(matches!(
        session_guard::notify(&store, &broker, OWNER, 6000)
            .await
            .unwrap(),
        Some(Notice::Dead { .. })
    ));
    assert_eq!(broker.count(), 1);

    // The same holds for the recovery notice.
    session_guard::record_success(&store, OWNER, 7000).unwrap();
    broker.fail.store(true, Ordering::SeqCst);
    assert!(session_guard::notify(&store, &broker, OWNER, 7000)
        .await
        .is_err());
    broker.fail.store(false, Ordering::SeqCst);
    assert_eq!(
        session_guard::notify(&store, &broker, OWNER, 8000)
            .await
            .unwrap(),
        Some(Notice::Recovered)
    );
    assert_eq!(broker.count(), 2);
}

#[tokio::test]
async fn a_session_that_dies_again_before_the_recovery_notice_stays_reported_dead() {
    let store = Store::open(":memory:").unwrap();
    let broker = Broker::default();
    session_guard::record_failure(&store, OWNER, "redirected", 1000).unwrap();
    session_guard::notify(&store, &broker, OWNER, 1000)
        .await
        .unwrap();
    session_guard::record_success(&store, OWNER, 2000).unwrap();
    session_guard::record_failure(&store, OWNER, "redirected", 3000).unwrap();
    // The owner's last word is "dead", which is still true: no false recovery
    // and no second alarm inside the day.
    assert_eq!(
        session_guard::notify(&store, &broker, OWNER, 4000)
            .await
            .unwrap(),
        None
    );
    assert_eq!(broker.count(), 1);
    // A success that started before the session died cannot clear it.
    session_guard::record_success(&store, OWNER, 2500).unwrap();
    assert_eq!(session_guard::dead_sessions(&store).unwrap().len(), 1);
}

/// The daemon's inbound source is what polls in production.
#[tokio::test]
async fn daemon_poll_alerts_the_owner_exactly_once_for_a_redirecting_session() {
    let healthy = Arc::new(AtomicBool::new(false));
    let server = server(healthy.clone()).await;
    let store = Arc::new(Store::open(":memory:").unwrap());
    let broker = Arc::new(Broker::default());
    let api = Arc::new(VoyagerClient::for_test(auth(), server.uri()).unwrap());
    let inbound = LinkedInInbound::new(api, OWNER.into())
        .with_store(store.clone())
        .with_session_notices(broker.clone());
    for _ in 0..3 {
        assert!(inbound.fetch_new().await.unwrap().is_empty());
    }
    assert_eq!(broker.count(), 1);
    assert_eq!(
        requests(&server).await,
        1,
        "the dead cookie is not replayed"
    );

    healthy.store(true, Ordering::SeqCst);
    assert!(session_guard::clear_pause_after_login(&store, OWNER).unwrap());
    inbound.fetch_new().await.unwrap();
    inbound.fetch_new().await.unwrap();
    assert_eq!(broker.count(), 2);
    assert!(broker.last().contains("reading again"));
}
