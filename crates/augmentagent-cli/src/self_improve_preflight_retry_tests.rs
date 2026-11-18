//! Exercise native worker scheduling/accounting before any issue workflow exists.
use super::*;
use std::sync::{Mutex, atomic::{AtomicUsize, Ordering::SeqCst}};

fn worker(state: &Path, start: tokio::time::Instant, base: chrono::DateTime<chrono::Utc>) -> AutoPrLoop {
    let mut worker = AutoPrLoop::for_tests("/nonexistent".into(), 5, state);
    worker.dry_run = false;
    worker.command_repository = Some("owner/repo".into());
    worker.clock = Arc::new(move || base + chrono::Duration::from_std(start.elapsed()).unwrap());
    worker
}
fn blocked() -> anyhow::Error {
    anyhow::anyhow!("scripted missing provider declaration").context(targeted::CommandFailure {
        code: 3, reason: "execution-readiness-blocked",
    })
}

#[tokio::test(start_paused = true)]
async fn pre_workflow_readiness_rechecks_every_five_minutes_without_compute_or_merge_debit() {
    let state = tempfile::tempdir().unwrap();
    let start = tokio::time::Instant::now();
    let base = chrono::Utc::now();
    let times = Arc::new(Mutex::new(Vec::new()));
    let observed = times.clone();
    let stop = tokio_util::sync::CancellationToken::new();
    let done = stop.clone();
    let run = worker(state.path(), start, base).run_builds(stop, move || {
        let mut times = observed.lock().unwrap();
        times.push(start.elapsed().as_secs());
        let last = times.len() == 4;
        let done = done.clone();
        async move { if last { done.cancel(); } Err(blocked()) }
    });
    tokio::select! {
        result = run => result.unwrap(),
        _ = tokio::time::sleep(std::time::Duration::from_secs(2800)) => {},
    }
    let times = times.lock().unwrap();
    assert_eq!(times.len(), 4, "pre-workflow faults slept through five-minute observation");
    assert!(times.windows(2).all(|t| t[1] - t[0] == 300), "{times:?}");
    let mut counter = DailyCounter::load(&state.path().join("autopr-daily-runs.json"));
    assert_eq!(counter.runs_today(utc_day_of(base)), 0, "read-only readiness is not paid work");
    let mut store = crate::autoship_workflow::WorkflowStore::open(&state.path().join("autopr-workflow.sqlite3")).unwrap();
    let (events, complete) = store.latest_workflow_events(base.timestamp_millis()).unwrap();
    assert!(complete && events.is_empty(), "preflight must not invent an issue workflow");
    assert_eq!(store.merge_allowance(base.timestamp_millis(), 5).unwrap().available, 5);
}

#[tokio::test(start_paused = true)]
async fn preflight_reconciliation_deadline_survives_worker_restart() {
    let state = tempfile::tempdir().unwrap();
    let start = tokio::time::Instant::now();
    let base = chrono::Utc::now();
    let stop = tokio_util::sync::CancellationToken::new();
    let done = stop.clone();
    worker(state.path(), start, base).run_builds(stop, move || {
        let done = done.clone();
        async move { done.cancel(); Err(blocked()) }
    }).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(150)).await;
    let restarted_at = tokio::time::Instant::now();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let stop = tokio_util::sync::CancellationToken::new();
    let done = stop.clone();
    let run = worker(state.path(), start, base).run_builds(stop, move || {
        let observed = observed.clone(); let done = done.clone();
        async move { observed.fetch_add(1, SeqCst); done.cancel(); Ok(RunReport::idle()) }
    });
    tokio::select! {
        result = run => result.unwrap(),
        _ = tokio::time::sleep(std::time::Duration::from_secs(151)) => {},
    }
    assert_eq!(calls.load(SeqCst), 1, "restart discarded the preflight observation deadline");
    assert_eq!(restarted_at.elapsed().as_secs(), 150);
    let counter = DailyCounter::load(&state.path().join("autopr-daily-runs.json"));
    assert!(counter.preflight_observation.is_none(), "recovery kept the old readiness schedule");
}

#[tokio::test(start_paused = true)]
async fn unrelated_error_text_does_not_enable_preflight_retries_or_refund_attempts() {
    let state = tempfile::tempdir().unwrap();
    let start = tokio::time::Instant::now(); let base = chrono::Utc::now();
    let calls = Arc::new(AtomicUsize::new(0)); let observed = calls.clone();
    let run = worker(state.path(), start, base).run_builds(tokio_util::sync::CancellationToken::new(), move || {
        observed.fetch_add(1, SeqCst);
        async { anyhow::bail!("execution-readiness-blocked: untyped error after paid work") }
    });
    tokio::select! {
        result = run => result.unwrap(),
        _ = tokio::time::sleep(std::time::Duration::from_secs(2200)) => {},
    }
    assert_eq!(calls.load(SeqCst), 1, "prose accidentally authorized fast retry");
    let mut counter = DailyCounter::load(&state.path().join("autopr-daily-runs.json"));
    assert_eq!(counter.runs_today(utc_day_of(base)), 1);
    assert!(counter.preflight_observation.is_none());
}
