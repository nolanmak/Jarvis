//! Backfill reads slowly and inside a durable request budget (#2255).
use augmentagent_channel_linkedin::{
    archive::ArchiveReader,
    archive_sync,
    backfill_pace::{self, Admit, Pacer, Pacing},
};
use augmentagent_store::Store;
use std::sync::{
    atomic::{AtomicI64, Ordering},
    Arc, Mutex,
};
use tokio_util::sync::CancellationToken;
use wiremock::{matchers::method, Mock, MockServer};
mod support;
use support::*;

const HOUR: i64 = 60 * 60 * 1000;
const DAY: i64 = 24 * HOUR;

/// A clock that only moves when the pacer sleeps, so no test really waits.
struct Fake {
    clock: Arc<AtomicI64>,
    sleeps: Arc<Mutex<Vec<i64>>>,
}
impl Fake {
    fn at(start: i64) -> Self {
        Self {
            clock: Arc::new(AtomicI64::new(start)),
            sleeps: Arc::default(),
        }
    }
    fn pacer(&self, pacing: Pacing) -> Pacer {
        let (clock, advance, sleeps) =
            (self.clock.clone(), self.clock.clone(), self.sleeps.clone());
        let draws = AtomicI64::new(0);
        Pacer::with_parts(
            pacing,
            move || clock.load(Ordering::SeqCst),
            move |ms| {
                sleeps.lock().unwrap().push(ms);
                advance.fetch_add(ms, Ordering::SeqCst);
                Box::pin(async {})
            },
            // Deterministic spread over the whole jitter range.
            move |max| (draws.fetch_add(1, Ordering::SeqCst) * 997) % (max + 1),
        )
    }
    fn now(&self) -> i64 {
        self.clock.load(Ordering::SeqCst)
    }
}

fn request_times(store: &Store) -> Vec<i64> {
    store
        .with_conn(|c| {
            c.prepare(
                "SELECT at_ms FROM linkedin_archive_requests WHERE account=?1 ORDER BY at_ms,rowid",
            )?
            .query_map([OWNER], |r| r.get(0))?
            .collect()
        })
        .unwrap()
}

async fn corpus() -> (MockServer, ArchiveReader) {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(respond)
        .mount(&server)
        .await;
    let reader = ArchiveReader::for_test(auth(), server.uri()).unwrap();
    (server, reader)
}

async fn requests(server: &MockServer) -> usize {
    server.received_requests().await.unwrap().len()
}

fn unlimited(delay_ms: i64, jitter_ms: i64) -> Pacing {
    Pacing {
        delay_ms,
        jitter_ms,
        hourly: 10_000,
        daily: 10_000,
    }
}

#[tokio::test]
async fn consecutive_backfill_requests_keep_the_configured_delay() {
    let (server, reader) = corpus().await;
    let store = Store::open(":memory:").unwrap();
    let fake = Fake::at(1_000_000);
    let pacer = fake.pacer(unlimited(5000, 3000));
    let report = archive_sync::sync_paced(
        &reader,
        &store,
        "PRIMARY_INBOX",
        12,
        &CancellationToken::new(),
        &pacer,
    )
    .await
    .unwrap();
    assert_eq!((report.pages, report.stopped.as_str()), (12, "page_budget"));
    assert_eq!(requests(&server).await, 12);
    let times = request_times(&store);
    assert_eq!(times.len(), 12);
    let gaps: Vec<i64> = times.windows(2).map(|pair| pair[1] - pair[0]).collect();
    assert!(
        gaps.iter().all(|gap| (5000..=8000).contains(gap)),
        "{gaps:?}"
    );
    assert!(
        gaps.iter().any(|gap| *gap > 5000),
        "jitter is applied: {gaps:?}"
    );
    assert_eq!(fake.sleeps.lock().unwrap().len(), 11);

    // A second invocation straight after still waits out the gap.
    let again = fake.pacer(unlimited(5000, 0));
    archive_sync::sync_paced(
        &reader,
        &store,
        "PRIMARY_INBOX",
        1,
        &CancellationToken::new(),
        &again,
    )
    .await
    .unwrap();
    let times = request_times(&store);
    assert_eq!(times[12] - times[11], 5000);
}

#[tokio::test]
async fn hourly_budget_stops_backfill_and_holds_across_restarts() {
    let (server, reader) = corpus().await;
    let file = tempfile::NamedTempFile::new().unwrap();
    let store = Store::open(file.path()).unwrap();
    let fake = Fake::at(1_000_000);
    let pacing = Pacing {
        delay_ms: 1000,
        jitter_ms: 0,
        hourly: 5,
        daily: 8,
    };
    let cancel = CancellationToken::new();
    let first = archive_sync::sync_paced(
        &reader,
        &store,
        "PRIMARY_INBOX",
        100,
        &cancel,
        &fake.pacer(pacing),
    )
    .await
    .unwrap();
    assert_eq!((first.pages, first.stopped.as_str()), (5, "request_budget"));
    assert_eq!(first.resume_at_ms, Some(1_000_000 + HOUR));
    assert!(!first.account_deferred);
    assert_eq!(requests(&server).await, 5);

    // A new process before the resume time makes no requests at all.
    drop(store);
    let store = Store::open(file.path()).unwrap();
    fake.clock.store(1_000_000 + HOUR - 1, Ordering::SeqCst);
    let second = archive_sync::sync_paced(
        &reader,
        &store,
        "PRIMARY_INBOX",
        100,
        &cancel,
        &fake.pacer(pacing),
    )
    .await
    .unwrap();
    assert_eq!(
        (second.pages, second.stopped.as_str()),
        (0, "request_budget")
    );
    assert_eq!(second.resume_at_ms, Some(1_000_000 + HOUR));
    assert_eq!(requests(&server).await, 5);
    assert!(
        fake.sleeps.lock().unwrap().len() <= 4,
        "no sleeping on a spent budget"
    );

    // At the resume time the sliding hourly window admits exactly one request.
    fake.clock.store(1_000_000 + HOUR, Ordering::SeqCst);
    let third = archive_sync::sync_paced(
        &reader,
        &store,
        "PRIMARY_INBOX",
        100,
        &cancel,
        &fake.pacer(pacing),
    )
    .await
    .unwrap();
    assert_eq!((third.pages, third.stopped.as_str()), (1, "request_budget"));
    assert_eq!(third.resume_at_ms, Some(1_001_000 + HOUR));

    // An hour later the daily budget (8) is what stops the run.
    fake.clock.store(1_000_000 + 2 * HOUR, Ordering::SeqCst);
    let fourth = archive_sync::sync_paced(
        &reader,
        &store,
        "PRIMARY_INBOX",
        100,
        &cancel,
        &fake.pacer(pacing),
    )
    .await
    .unwrap();
    assert_eq!(
        (fourth.pages, fourth.stopped.as_str()),
        (2, "request_budget")
    );
    assert_eq!(fourth.resume_at_ms, Some(1_000_000 + DAY));
    assert_eq!(requests(&server).await, 8);
    // Every paced request committed a page: the import resumed, not restarted.
    assert_eq!(fourth.coverage.mailbox_pages.committed, Some(3));
}

#[tokio::test]
async fn live_reads_come_out_of_the_same_budget_and_are_never_held() {
    let (server, reader) = corpus().await;
    let store = Store::open(":memory:").unwrap();
    let fake = Fake::at(50_000_000);
    let now = || fake.now();
    let cancel = CancellationToken::new();
    let pacing = Pacing {
        delay_ms: 1000,
        jitter_ms: 0,
        hourly: 5,
        daily: 100,
    };
    // The live poll has already read three pages this hour.
    let live = archive_sync::sync_with_clock(&reader, &store, "PRIMARY_INBOX", 3, &cancel, &now)
        .await
        .unwrap();
    assert_eq!(live.pages, 3);
    let backfill = archive_sync::sync_paced(
        &reader,
        &store,
        "PRIMARY_INBOX",
        100,
        &cancel,
        &fake.pacer(pacing),
    )
    .await
    .unwrap();
    assert_eq!(
        (backfill.pages, backfill.stopped.as_str()),
        (2, "request_budget")
    );
    assert_eq!(requests(&server).await, 5);
    // With the backfill budget spent, the live poll still reads at once.
    let sleeps = fake.sleeps.lock().unwrap().len();
    let live = archive_sync::sync_with_clock(&reader, &store, "PRIMARY_INBOX", 4, &cancel, &now)
        .await
        .unwrap();
    assert_eq!((live.pages, live.stopped.as_str()), (4, "page_budget"));
    assert_eq!(live.resume_at_ms, None);
    assert_eq!(fake.sleeps.lock().unwrap().len(), sleeps);
    assert_eq!(requests(&server).await, 9);
}

#[tokio::test]
async fn cancelling_during_the_delay_stops_without_a_request() {
    let (server, reader) = corpus().await;
    let store = Store::open(":memory:").unwrap();
    backfill_pace::record_request(&store, OWNER, 1000).unwrap();
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let pacer = Pacer::with_parts(
        unlimited(5000, 0),
        || 1000,
        move |_| {
            trigger.cancel();
            Box::pin(std::future::pending())
        },
        |_| 0,
    );
    assert_eq!(
        pacer.admit(&store, OWNER, &cancel).await.unwrap(),
        Admit::Cancelled
    );
    let report = archive_sync::sync_paced(&reader, &store, "PRIMARY_INBOX", 5, &cancel, &pacer)
        .await
        .unwrap();
    assert_eq!((report.pages, report.stopped.as_str()), (0, "cancelled"));
    assert_eq!(requests(&server).await, 0);
}

#[test]
fn the_request_ledger_keeps_one_day_and_reports_the_later_window() {
    let store = Store::open(":memory:").unwrap();
    let pacing = Pacing {
        delay_ms: 0,
        jitter_ms: 0,
        hourly: 2,
        daily: 3,
    };
    assert_eq!(
        backfill_pace::budget_resume_at(&store, OWNER, &pacing, 0).unwrap(),
        None
    );
    for at in [10, 20, 30] {
        backfill_pace::record_request(&store, OWNER, at).unwrap();
    }
    // Both windows are spent; the daily one reopens later.
    assert_eq!(
        backfill_pace::budget_resume_at(&store, OWNER, &pacing, 40).unwrap(),
        Some(10 + DAY)
    );
    // Another account has its own budget.
    assert_eq!(
        backfill_pace::budget_resume_at(&store, "urn:li:fsd_profile:OTHER", &pacing, 40).unwrap(),
        None
    );
    backfill_pace::record_request(&store, OWNER, 30 + DAY).unwrap();
    assert_eq!(
        request_times(&store),
        vec![30 + DAY],
        "older rows are pruned"
    );
    assert!(Pacing::default().delay_ms >= 1000);
}
