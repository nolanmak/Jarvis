use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
#[tokio::test(start_paused = true)]
async fn a_paid_deferred_turn_serves_a_peer_before_the_poll_interval() {
    let state = tempfile::tempdir().unwrap();
    let mut worker = AutoPrLoop::for_tests("/nonexistent".into(), 5, state.path());
    worker.interval = std::time::Duration::from_secs(1);
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let stop = tokio_util::sync::CancellationToken::new();
    let timer_stop = stop.clone();
    let run = worker.run_with(
        stop,
        move || {
            let turn = observed.fetch_add(1, Ordering::SeqCst);
            async move {
                Ok(RunReport {
                    message: format!("fixture workflow {}", turn + 1),
                    billed: true,
                    held: turn == 0,
                    resumed: false,
                })
            }
        },
        || async { 0 },
    );
    let timer = async move {
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        timer_stop.cancel();
    };
    let (result, ()) = tokio::join!(run, timer);
    result.unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "paid retry monopolized the serial run slot until the next interval"
    );
}

#[tokio::test(start_paused = true)]
async fn a_due_retry_wakes_the_worker_before_the_regular_poll() {
    use crate::autoship_workflow::{Binding, NextAction, Stage, WorkflowStore};
    let state = tempfile::tempdir().unwrap();
    let mut worker = AutoPrLoop::for_tests("/nonexistent".into(), 5, state.path());
    worker.dry_run = false;
    worker.command_repository = Some("owner/repo".into());
    let path = worker.workflow_path.clone();
    let stop = tokio_util::sync::CancellationToken::new();
    let timer_stop = stop.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let create = async move {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        let now = chrono::Utc::now().timestamp_millis();
        let mut store = WorkflowStore::open(&path).unwrap();
        store
            .register(
                "owner/repo#1",
                "candidate",
                Binding {
                    head: "head".into(),
                    contract: "contract".into(),
                    evidence_hash: "witness".into(),
                },
                now,
            )
            .unwrap();
        let lease = store
            .claim("owner/repo#1", "fixture", now, 1000)
            .unwrap()
            .unwrap();
        let mut cp = store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
        cp.stage = Stage::Scope;
        cp.next = NextAction {
            reason: "scope_deferred".into(),
            action: "retry_scope".into(),
            eligible_at_ms: now,
        };
        store.checkpoint(&lease, &cp, now).unwrap();
        store.release(&lease, now).unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(26)).await;
        timer_stop.cancel();
    };
    let run = worker.run_with(
        stop,
        move || {
            observed.fetch_add(1, Ordering::SeqCst);
            async { Ok(RunReport::idle()) }
        },
        || async { 0 },
    );
    let (result, ()) = tokio::join!(run, create);
    result.unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "due retry slept through the 30-second wake poll"
    );
}
