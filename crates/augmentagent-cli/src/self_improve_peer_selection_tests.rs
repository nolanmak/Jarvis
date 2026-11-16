//! Real worker wake + real candidate selection; scripted GitHub, no model call.
use super::*;
use std::sync::{Mutex, atomic::{AtomicUsize, Ordering::SeqCst}};

pub(super) async fn worker_selects_real_ready_peer(repo: &Path, database: &Path) {
    let mut store = crate::autoship_workflow::WorkflowStore::open(database).unwrap();
    let ids = ["fixture-owner/repo#1833", "fixture-owner/repo#1830"];
    let before = ids.iter().map(|id| (store.snapshot(id).unwrap(), store.events(id).unwrap())).collect::<Vec<_>>();
    assert!(!store.has_due_qualification_recovery(Some("fixture-owner/repo"), chrono::Utc::now().timestamp_millis()).unwrap(),
        "fixture accidentally enabled the separate qualification-recovery wake");
    for (index, id) in ids.iter().enumerate() {
        assert_eq!(before[index].0.as_ref().unwrap().checkpoint.next.action,
            if id.ends_with("1833") { "await_readiness_input_change" } else { "await_acceptance_input_change" });
    }
    let state = tempfile::tempdir().unwrap();
    let mut worker = AutoPrLoop::for_tests(repo.to_owned(), 5, state.path());
    worker.dry_run = false;
    worker.command_repository = Some("fixture-owner/repo".into());
    worker.workflow_path = database.to_owned();
    tokio::time::pause();
    let start = tokio::time::Instant::now(); let base = chrono::Utc::now();
    worker.clock = Arc::new(move || base + chrono::Duration::from_std(start.elapsed()).unwrap());
    let stop = tokio_util::sync::CancellationToken::new(); let done = stop.clone();
    let turns = Arc::new(AtomicUsize::new(0)); let attempts = turns.clone();
    let selected = Arc::new(Mutex::new(None)); let observed = selected.clone();
    let failed_at = Arc::new(Mutex::new(None)); let failure = failed_at.clone();
    let root = repo.to_owned();
    let run = worker.run_builds(stop, move || {
        let (root, turns, observed, failure, done) = (root.clone(), attempts.clone(), observed.clone(), failure.clone(), done.clone());
        async move {
            if turns.fetch_add(1, SeqCst) == 0 {
                *failure.lock().unwrap() = Some(start.elapsed());
                anyhow::bail!("scripted attempt failed; original native input holds already persisted");
            }
            // External Git/GitHub fixture processes use real IO. Pause only
            // worker scheduling, never manufacture an IO timeout as RED.
            tokio::time::resume();
            let pick = select_issue_with_fallback(&root, true, true,
                || async { anyhow::bail!("ready ordinary peer yielded to waiting-work fallback") }).await;
            tokio::time::pause();
            let issue = match pick? {
                IssueSelection::Issue(issue) => issue,
                IssueSelection::Report(_) => anyhow::bail!("real picker did not return an issue"),
            };
            *observed.lock().unwrap() = Some((issue.number, start.elapsed()));
            done.cancel();
            Ok(RunReport::held("real ordinary peer selected; no paid dispatch".into()))
        }
    });
    tokio::select! {
        result = run => result.unwrap(),
        _ = tokio::time::sleep(std::time::Duration::from_secs(2401)) => {},
    }
    tokio::time::resume();
    let (number, at) = selected.lock().unwrap().expect("worker never selected the real ready peer within two observation cycles");
    assert_eq!(number, 1829, "worker selected a retained unchanged hold instead of the ordinary peer");
    assert!(at - failed_at.lock().unwrap().unwrap() <= std::time::Duration::from_secs(600));
    assert_eq!(turns.load(SeqCst), 2, "unchanged held work monopolized scheduling turns");
    for (index, id) in ids.iter().enumerate() {
        assert_eq!(store.snapshot(id).unwrap(), before[index].0, "observation/selection reclaimed or changed a held workflow");
        let semantic = store.events(id).unwrap().into_iter().filter(|event| event.1 != "activity_observation_scheduled").collect::<Vec<_>>();
        assert_eq!(semantic, before[index].1, "observation/selection manufactured workflow progress");
        assert_eq!(store.snapshot(id).unwrap().unwrap().model_calls, 0);
    }
    assert!(store.snapshot("fixture-owner/repo#1829").unwrap().is_none(), "selection executed a model workflow");
    assert_eq!(store.merge_allowance(chrono::Utc::now().timestamp_millis(), 5).unwrap().available, 5);
    let calls = std::fs::read_to_string(std::env::var_os("AUTOSHIP_RECOVERED_SELECTION_CALLS").unwrap()).unwrap();
    for operation in ["\"edit\"", "\"comment\"", "\"merge\""] {
        assert!(!calls.contains(operation), "selection mutated remote state");
    }
}
