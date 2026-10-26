//! Cross-version queue recovery. Only APIs present in the pinned relay baseline
//! are used, so this exact fixture can execute in both versions.
use augmentagent_store::{
    phone::{CallState, ReserveCall},
    Store,
};
use rusqlite::Connection;
use std::path::Path;

fn rows(path: &Path, tables: &[&str]) -> Vec<Vec<Vec<String>>> {
    let db = Connection::open(path).unwrap();
    tables
        .iter()
        .map(|table| {
            let mut q = db
                .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
                .unwrap();
            let n = q.column_count();
            q.query_map([], |row| {
                (0..n)
                    .map(|i| Ok(format!("{:?}", row.get_ref(i)?)))
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
        })
        .collect()
}
const LEGACY: &[&str] = &["phone_calls", "phone_destinations", "phone_dispatch_jobs"];
const LIVE: &[&str] = &[
    "phone_live_sessions",
    "phone_live_dispatches",
    "phone_voice_backends",
    "phone_live_history_batches",
    "phone_live_transcripts",
    "phone_live_usage",
    "phone_live_summary_jobs",
];
fn seed_jobs(store: &Store, call: &str, prefix: &str) {
    for state in ["queued", "running", "ready"] {
        let task = format!("{prefix}-{state}");
        store
            .phone_enqueue_dispatch(call, &task, &format!("Synthetic {task}"))
            .unwrap();
        if state != "queued" {
            assert!(store.phone_start_dispatch(call, &task).unwrap());
        }
        if state == "ready" {
            store
                .phone_finish_dispatch(call, &task, "Persisted full chat result")
                .unwrap();
        }
    }
}
#[test]
#[ignore = "requires isolated cross-version fixture driver; no providers or user data"]
fn upgrade_fixture() {
    let path = std::path::PathBuf::from(std::env::var_os("PHONE_UPGRADE_FIXTURE_DB").unwrap());
    let phase = std::env::var("PHONE_UPGRADE_FIXTURE_PHASE").unwrap();
    let legacy = path.with_extension("legacy.json");
    let live = path.with_extension("live.json");
    let store = Store::open(&path).unwrap();
    if phase == "seed" {
        store
            .phone_import_destination("owner", "+12025550123", 0)
            .unwrap();
        let call = store
            .phone_reserve_call(ReserveCall {
                owner: "owner",
                request_id: "seed",
                conversation: "synthetic-chat",
                now_ms: 0,
            })
            .unwrap()
            .call;
        store.phone_bind_provider(&call.id, "CA-synthetic").unwrap();
        store
            .phone_provider_event(&call.id, "CA-synthetic", CallState::AwaitingAcceptance, 1)
            .unwrap();
        store.phone_accept(&call.id, "CA-synthetic", 2).unwrap();
        store
            .phone_provider_event(&call.id, "CA-synthetic", CallState::Completed, 100)
            .unwrap();
        seed_jobs(&store, &call.id, "relay");
        std::fs::write(&legacy, serde_json::to_vec(&rows(&path, LEGACY)).unwrap()).unwrap();
        return;
    }
    assert!(matches!(
        phase.as_str(),
        "upgrade" | "rollback" | "reupgrade"
    ));
    let expected: Vec<Vec<Vec<String>>> =
        serde_json::from_slice(&std::fs::read(&legacy).unwrap()).unwrap();
    assert_eq!(
        rows(&path, LEGACY),
        expected,
        "opening another version changed durable calls or jobs"
    );
    let call: String = store
        .with_conn(|db| {
            db.query_row(
                "SELECT id FROM phone_calls WHERE request_id='seed'",
                [],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert!(
        store.phone_active_calls().unwrap().is_empty(),
        "rollback must not reactivate a call"
    );
    if phase == "upgrade" {
        seed_jobs(&store, &call, "live");
        store.with_conn(|db| {
            db.execute("INSERT INTO phone_live_sessions(call_id,session_id,active) VALUES(?1,'synthetic-live-session',0)", [&call])?;
            db.execute("INSERT INTO phone_voice_backends(call_id,backend) VALUES(?1,'openai_live')", [&call])?;
            for state in ["queued", "running", "ready"] {
                let task = format!("live-{state}");
                db.execute("INSERT INTO phone_live_dispatches(call_id,delegation_id,task_id,result) VALUES(?1,?2,?2,?3)", rusqlite::params![call,task,if state=="ready" {Some("Separate voice result")} else {None}])?;
            }
            db.execute("INSERT INTO phone_live_history_batches(id,call_id,text) VALUES('history-key',?1,'Private synthetic transcript')", [&call])?;
            db.execute("INSERT INTO phone_live_transcripts(call_id,event_id,fingerprint,user,text,batch_id) VALUES(?1,'event','fingerprint',1,'Private synthetic transcript','history-key')", [&call])?;
            db.execute("INSERT INTO phone_live_usage(call_id,phase,session_id,end_trigger,final_reason,seconds) VALUES(?1,'conversation','synthetic-live-session','twilio_stop','close_requested',2.5)", [&call])?;
            db.execute("INSERT INTO phone_live_summary_jobs(call_id,delegation_id,attempts,lease_token,next_ms) VALUES(?1,'live-ready',1,'synthetic-lease',10000)", [&call])?;
            Ok(())
        }).unwrap();
        std::fs::write(&live, serde_json::to_vec(&rows(&path, LIVE)).unwrap()).unwrap();
    } else {
        let expected: Vec<Vec<Vec<String>>> =
            serde_json::from_slice(&std::fs::read(&live).unwrap()).unwrap();
        assert_eq!(
            rows(&path, LIVE),
            expected,
            "rollback lost Live state or mixed audio receipts with chat delivery"
        );
        if phase == "rollback" {
            assert_eq!(store.phone_pending_dispatches().unwrap().len(), 6);
            for prefix in ["relay", "live"] {
                assert!(
                    !store
                        .phone_start_dispatch(&call, &format!("{prefix}-running"))
                        .unwrap(),
                    "uncertain running work must not replay"
                );
                assert!(
                    !store
                        .phone_start_dispatch(&call, &format!("{prefix}-ready"))
                        .unwrap(),
                    "result delivery must not rerun tools"
                );
                let queued = format!("{prefix}-queued");
                assert!(store.phone_start_dispatch(&call, &queued).unwrap());
                assert!(!store.phone_start_dispatch(&call, &queued).unwrap());
                store
                    .phone_finish_dispatch(&call, &queued, "Recovered result")
                    .unwrap();
                store
                    .phone_dispatch_delivered(&call, &format!("{prefix}-ready"))
                    .unwrap();
            }
        } else {
            let jobs = store.phone_pending_dispatches().unwrap();
            assert_eq!(jobs.len(), 4);
            for job in jobs {
                assert!(!store.phone_start_dispatch(&call, &job.task_id).unwrap());
                if job.task_id.ends_with("-queued") {
                    assert_eq!(job.state, "ready");
                    assert_eq!(job.result.as_deref(), Some("Recovered result"));
                } else {
                    assert_eq!(job.state, "running");
                }
            }
        }
    }
    std::fs::write(&legacy, serde_json::to_vec(&rows(&path, LEGACY)).unwrap()).unwrap();
}
