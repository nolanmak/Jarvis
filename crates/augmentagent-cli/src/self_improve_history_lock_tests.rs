//! Real production history writers must join the same read/modify/write lock.
use super::*;
use std::os::{fd::AsRawFd, unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt}};

const CHILD_ROOT: &str = "AUTOSHIP_HISTORY_LOCK_CHILD_ROOT";

fn record(stage: &str) -> AttemptRecord {
    AttemptRecord {
        at: 1, kind: FailureKind::Infra, stage: stage.into(),
        detail: "synthetic infrastructure observation".into(), diffstat: String::new(),
        diff_lines: 0, wall_secs: 0, reasoner_calls: 0, providers: String::new(),
    }
}

fn wait_for(path: &Path, budget: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + budget;
    while !path.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    path.exists()
}

fn finish(mut child: std::process::Child) -> std::process::Output {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if child.try_wait().unwrap().is_some() { return child.wait_with_output().unwrap(); }
        if std::time::Instant::now() >= deadline {
            // This is our disposable fixture child, not a production runtime.
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!("history fixture did not exit after lock release: {}", String::from_utf8_lossy(&output.stderr));
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

async fn locked_writer(mode: &str, test: &str) {
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        let root = PathBuf::from(root);
        std::fs::write(root.join("entered"), b"original writer about to run").unwrap();
        match mode {
            "reasoner" => record_reasoner_error(7, record("child")),
            "attempt" => assert_eq!(record_attempt(&root, 7, Some(record("child"))).await.unwrap().n, 0),
            "readmit" => assert_eq!(readmit(&root, "gate-red", false, true).await.unwrap(), vec![7]),
            _ => panic!("unexpected fixture mode"),
        }
        std::fs::write(root.join("finished"), b"original writer returned").unwrap();
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("history.json");
    let mut seed = AttemptHistory::default();
    seed.push(1, record("unrelated")); seed.push(7, record("seed")); seed.save(&path);
    let before = std::fs::read(&path).unwrap();
    let lock_path = root.path().join("history.json.lock");
    let guard = std::fs::OpenOptions::new().write(true).create(true).truncate(false)
        .mode(0o600).open(&lock_path).unwrap();
    assert_eq!(unsafe { libc::flock(guard.as_raw_fd(), libc::LOCK_EX) }, 0);
    let original_inode = guard.metadata().unwrap().ino();
    let gh = root.path().join("gh");
    std::fs::write(root.path().join("scenario.json"), serde_json::to_vec(&serde_json::json!({
        "label": GAVE_UP_LABEL,
        "reason": format!("{GAVE_UP_REASON_MARKER_PREFIX}gate-red -->"),
    })).unwrap()).unwrap();
    std::fs::write(&gh, r#"#!/usr/bin/python3
import json, pathlib, sys
root = pathlib.Path(__file__).parent
scenario = json.loads((root / 'scenario.json').read_text())
a = sys.argv[1:]
with (root / 'gh-calls.jsonl').open('a') as out:
    out.write(json.dumps(a) + '\n')
if a[:2] == ['issue', 'list']:
    print(json.dumps([{'number': 7, 'labels': [{'name': scenario['label']}]}]))
elif a and a[0] == 'api':
    print(json.dumps([{'id': 1, 'body': scenario['reason']}]))
elif a[:2] == ['issue', 'comment']:
    print('ok')
elif a[:2] == ['issue', 'edit']:
    (root / 'at-history-boundary').write_text('last remote step finished')
    print('ok')
else:
    raise SystemExit(91)
"#).unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env_clear().env("PATH", "/usr/bin:/bin").env("HOME", root.path())
        .env("XDG_STATE_HOME", root.path().join("state"))
        .env(CHILD_ROOT, root.path()).env("GH_BIN", &gh)
        .env("AUGMENTAGENT_AUTOPR_HISTORY_FILE", &path)
        .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped())
        .spawn().unwrap();
    let entry = if mode == "readmit" { "at-history-boundary" } else { "entered" };
    if !wait_for(&root.path().join(entry), std::time::Duration::from_secs(10)) {
        let _ = child.kill();
        let output = child.wait_with_output().unwrap();
        panic!("fixture did not enter writer: {}", String::from_utf8_lossy(&output.stderr));
    }
    let returned_early = wait_for(&root.path().join("finished"), std::time::Duration::from_secs(2));
    let during = std::fs::read(&path).unwrap();
    // Commit a peer change while the child should still be waiting. Loading
    // before locking would erase this even if replacement itself were locked.
    let mut peer = AttemptHistory::load(&path);
    peer.push(99, record("peer committed while child waited")); peer.save(&path);
    drop(guard);
    let output = finish(child);
    assert!(output.status.success(), "{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    assert!(!returned_early, "{mode} production writer ignored the shared history lock");
    assert_eq!(during, before, "history changed before the original lock was released");
    assert_eq!(std::fs::metadata(&lock_path).unwrap().ino(), original_inode, "lock inode must stay stable");
    let after = AttemptHistory::load(&path);
    assert!(after.issues.contains_key(&1));
    assert!(after.issues.contains_key(&99), "writer loaded a stale snapshot before taking the lock");
    if mode == "readmit" {
        assert!(!after.issues.contains_key(&7));
        let calls: Vec<Vec<String>> = std::fs::read_to_string(root.path().join("gh-calls.jsonl")).unwrap()
            .lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        let reset = calls.iter().position(|args| args.first().map(String::as_str) == Some("issue")
            && args.get(1).map(String::as_str) == Some("comment")).unwrap();
        let unlabel = calls.iter().position(|args| args.first().map(String::as_str) == Some("issue")
            && args.get(1).map(String::as_str) == Some("edit")).unwrap();
        assert!(reset < unlabel, "readmission must keep reset marker before unlabelling");
        assert!(calls[reset].iter().any(|arg| arg.contains(ATTEMPT_RESET_MARKER)));
    }
    else {
        assert_eq!(after.issues[&7].len(), 2);
        assert_eq!(after.issues[&7].last().unwrap().stage, "child");
        assert!(!root.path().join("gh-calls.jsonl").exists(), "harness failures must not charge a GitHub marker");
    }
}

#[tokio::test]
async fn reasoner_error_waits_for_history_lock_and_keeps_peer_update() {
    locked_writer("reasoner", "self_improve::history_lock_tests::reasoner_error_waits_for_history_lock_and_keeps_peer_update").await;
}
#[tokio::test]
async fn infrastructure_attempt_waits_for_history_lock_and_keeps_peer_update() {
    locked_writer("attempt", "self_improve::history_lock_tests::infrastructure_attempt_waits_for_history_lock_and_keeps_peer_update").await;
}
#[tokio::test]
async fn readmission_reset_waits_for_history_lock_and_keeps_peer_update() {
    locked_writer("readmit", "self_improve::history_lock_tests::readmission_reset_waits_for_history_lock_and_keeps_peer_update").await;
}

#[test]
fn untrusted_history_lock_refuses_edit_without_unlocked_fallback() {
    use std::os::unix::{ffi::OsStrExt, fs::symlink};
    for kind in ["symlink", "hardlink", "fifo", "permissive"] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("history.json");
        let mut seed = AttemptHistory::default(); seed.push(1, record("original")); seed.save(&path);
        let before = std::fs::read(&path).unwrap();
        let lock = root.path().join("history.json.lock");
        let sentinel = root.path().join("sentinel");
        std::fs::write(&sentinel, b"untouched").unwrap();
        std::fs::set_permissions(&sentinel, std::fs::Permissions::from_mode(0o600)).unwrap();
        match kind {
            "symlink" => symlink(&sentinel, &lock).unwrap(),
            "hardlink" => std::fs::hard_link(&sentinel, &lock).unwrap(),
            "fifo" => {
                let name = std::ffi::CString::new(lock.as_os_str().as_bytes()).unwrap();
                assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
            }
            "permissive" => {
                std::fs::write(&lock, b"").unwrap();
                std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o644)).unwrap();
            }
            _ => unreachable!(),
        }
        let mut edited = false;
        assert!(AttemptHistory::update_persist(&path, |_| edited = true).is_err(), "{kind}");
        assert!(!edited, "{kind} must refuse before editing");
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"untouched");
        assert!(std::fs::symlink_metadata(&lock).is_ok(), "refusal must preserve the original lock path");
    }
}

#[test]
fn failed_history_publication_preserves_committed_context_and_lock() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("history.json");
    let mut seed = AttemptHistory::default(); seed.push(1, record("original")); seed.save(&path);
    let before = std::fs::read(&path).unwrap();
    std::fs::create_dir(root.path().join("history.json.tmp")).unwrap();
    assert!(AttemptHistory::update_persist(&path, |h| h.push(7, record("unpublished"))).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(root.path().join("history.json.tmp").is_dir());
    assert!(root.path().join("history.json.lock").is_file());
    assert!(!root.path().join("history.json.bak").exists());
}

#[test]
fn killed_fixture_writer_releases_same_lock_inode_without_publishing_its_edit() {
    use std::os::unix::process::ExitStatusExt;
    const TEST: &str = "self_improve::history_lock_tests::killed_fixture_writer_releases_same_lock_inode_without_publishing_its_edit";
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        let root = PathBuf::from(root);
        AttemptHistory::update_persist(&root.join("history.json"), |history| {
            history.push(100, record("uncommitted fixture edit"));
            std::fs::write(root.join("lock-held"), b"original child owns lock").unwrap();
            loop { std::thread::park(); }
        }).unwrap();
        unreachable!();
    }
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("history.json");
    let mut seed = AttemptHistory::default(); seed.push(7, record("committed")); seed.save(&path);
    let before = std::fs::read(&path).unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", TEST, "--nocapture"]).env_clear()
        .env("PATH", "/usr/bin:/bin").env("HOME", root.path()).env(CHILD_ROOT, root.path())
        .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
    if !wait_for(&root.path().join("lock-held"), std::time::Duration::from_secs(10)) {
        let _ = child.kill(); let output = child.wait_with_output().unwrap();
        panic!("fixture child never owned lock: {}", String::from_utf8_lossy(&output.stderr));
    }
    let lock = root.path().join("history.json.lock");
    let original_inode = std::fs::metadata(&lock).unwrap().ino();
    let while_held = std::fs::read(&path).unwrap();
    // Only this original disposable child is terminated. Confirm death before
    // testing kernel flock release; no production/native lease is inferred.
    child.kill().unwrap();
    let status = child.wait().unwrap();
    assert_eq!(status.signal(), Some(libc::SIGKILL));
    assert_eq!(while_held, before);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    AttemptHistory::update_persist(&path, |h| h.push(9, record("after confirmed fixture death"))).unwrap();
    assert_eq!(std::fs::metadata(&lock).unwrap().ino(), original_inode);
    let after = AttemptHistory::load(&path);
    assert!(after.issues.contains_key(&7)); assert!(after.issues.contains_key(&9));
    assert!(!after.issues.contains_key(&100));
    assert_eq!(std::fs::read(AttemptHistory::bak_path(&path)).unwrap(), before);
}
