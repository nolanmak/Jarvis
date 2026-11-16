//! Exercise the actual worker wait loop with a durable failed workflow and fake time.
use super::*;
use crate::autoship_workflow::{Binding, NextAction, Stage, WorkflowStore};

async fn observe_failed_tick(stop_after: usize, timeout_secs: u64) -> Vec<u64> {
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let output = std::process::Command::new("/usr/bin/git").args(["init", "-q"])
        .current_dir(root.path()).output().unwrap();
    assert!(output.status.success());
    let mut worker = AutoPrLoop::for_tests(root.path().to_owned(), 5, state.path());
    worker.dry_run = false;
    worker.interval = std::time::Duration::from_secs(1800);
    worker.command_repository = Some("owner/repo".into());
    let start = tokio::time::Instant::now();
    let base = chrono::Utc::now();
    worker.clock = Arc::new(move || base + chrono::Duration::from_std(
        tokio::time::Instant::now() - start).unwrap());
    let clock = worker.clock.clone();
    let database = worker.workflow_path.clone();
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed = attempts.clone();
    let times = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = times.clone();
    let stop = tokio_util::sync::CancellationToken::new();
    let done = stop.clone();
    let result = worker.run_builds(stop, move || {
        let (database, clock, count, times, done) =
            (database.clone(), clock.clone(), observed.clone(), recorded.clone(), done.clone());
        async move {
            times.lock().unwrap().push((tokio::time::Instant::now() - start).as_secs());
            let invocation = count.fetch_add(1, SeqCst) + 1;
            if invocation == 1 {
                let now = clock().timestamp_millis();
                let mut store = WorkflowStore::open(&database)?;
                let binding = Binding {head:"candidate".into(),contract:"owner-criteria".into(),evidence_hash:"evidence".into()};
                store.register("owner/repo#7", "candidate", binding, now)?;
                let lease = store.claim("owner/repo#7", "failed-controller", now, 60_000)?.unwrap();
                let mut checkpoint = store.snapshot(&lease.workflow)?.unwrap().checkpoint;
                checkpoint.stage = Stage::Scope;
                checkpoint.next = NextAction {reason:"acceptance_contract_invalid".into(),
                    action:"await_acceptance_input_change".into(),eligible_at_ms:0};
                store.checkpoint(&lease, &checkpoint, now)?;
                store.release(&lease, now)?;
                anyhow::bail!("native preflight/qualification failed; ready peer remains");
            }
            if invocation >= stop_after { done.cancel(); }
            Ok(RunReport::held("peer scheduling callback observed".into()))
        }
    });
    tokio::select! {
        result = result => result.unwrap(),
        _ = tokio::time::sleep(std::time::Duration::from_secs(timeout_secs)) => {}
    }
    assert_eq!(attempts.load(SeqCst), stop_after,
        "ready peer waited for the thirty-minute issue interval after a failed tick");
    let times = times.lock().unwrap().clone();
    let store = WorkflowStore::open(&state.path().join("autopr-workflow.sqlite3")).unwrap();
    let failed = store.snapshot("owner/repo#7").unwrap().unwrap();
    assert_eq!(failed.model_calls, 0);
    assert_eq!(failed.checkpoint.next.action, "await_acceptance_input_change");
    assert_eq!(failed.attempts, 1, "observation claimed the failed workflow again");
    let activity = store.activity_status("owner/repo#7").unwrap().unwrap();
    assert_eq!(activity.last_meaningful_progress_at, 0);
    assert!(activity.inactivity_deadline_at.is_none());
    times
}

#[tokio::test(start_paused = true)]
async fn failed_ordinary_work_cannot_delay_a_ready_peer_for_thirty_minutes() {
    let times = observe_failed_tick(2, 2401).await;
    assert!(times[1] - times[0] <= 600, "ready peer exceeded two observation cycles");
}

#[tokio::test(start_paused = true)]
async fn unchanged_wait_observation_does_not_spin_at_the_thirty_second_queue_poll() {
    let times = observe_failed_tick(3, 2701).await;
    assert_eq!(times[1] - times[0], 300);
    assert_eq!(times[2] - times[1], 300, "unchanged wait became a queue-poll retry storm");
}
