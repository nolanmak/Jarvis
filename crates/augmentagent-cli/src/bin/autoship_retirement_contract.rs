use std::fs::{self, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

fn write(path: &Path, value: &[u8]) {
    fs::write(path, value).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn session(provider: &Path, name: &str, owner: &str, extra: &[(&str, &[u8])]) -> PathBuf {
    let path = provider.join(name);
    fs::create_dir(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    write(&path.join("owned.json"), format!("{{\"version\":1,\"audit_path\":\"{owner}\",\"owner\":{{\"workflow_id\":\"workflow-a\",\"lease_generation\":7}}}}") .as_bytes());
    write(&path.join("retired.json"), b"{\"version\":1,\"reason\":\"scope_retired\"}");
    write(&path.join("complete.json"), b"{\"version\":1,\"generation\":\"g7\"}");
    write(&path.join("lock"), b"");
    for (name, value) in extra { write(&path.join(name), value); }
    path
}

fn alias(aliases: &Path, name: &str, target: &str) {
    write(&aliases.join(name), format!("\"{target}\"").as_bytes());
}

fn exists(path: &Path) -> bool { path.exists() }

fn main() {
    let root = std::env::temp_dir().join(format!("autoship-retirement-contract-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    let state = root.join("state");
    let provider = state.join("codex");
    fs::create_dir(&state).unwrap();
    fs::create_dir(&provider).unwrap();
    for path in [&state, &provider] { fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap(); }
    write(&state.join("state.lock"), b"");
    let aliases = provider.join("aliases");
    fs::create_dir(&aliases).unwrap();
    fs::set_permissions(&aliases, fs::Permissions::from_mode(0o700)).unwrap();

    let verified = session(&provider, "session-verified", "/audit/original", &[
        ("producer-completion.json", b"{\"version\":1,\"audit_path\":\"/audit/original\",\"owner\":{\"workflow_id\":\"workflow-a\",\"lease_generation\":7},\"writer_fence\":7}"),
        ("settled-result.json", b"{\"version\":1,\"owner\":{\"workflow_id\":\"workflow-a\",\"lease_generation\":7},\"durable\":true}"),
    ]);
    let incomplete = session(&provider, "session-incomplete", "/audit/original", &[]);
    let active = session(&provider, "session-active", "/audit/original", &[("active.json", b"{\"writer_fence\":7}")]);
    let resumed = session(&provider, "session-resumed", "/audit/original", &[("resumed.json", b"{\"owner\":\"original\"}")]);
    let mismatch = session(&provider, "session-mismatch", "/audit/original", &[("producer-completion.json", b"{\"version\":1,\"audit_path\":\"/audit/other\",\"owner\":{\"workflow_id\":\"other\",\"lease_generation\":8},\"writer_fence\":8}"), ("settled-result.json", b"{\"durable\":true}")]);
    let stale = session(&provider, "session-stale", "/audit/original", &[("lease.json", b"{\"expired\":true}")]);
    let missing = session(&provider, "session-missing-process", "/audit/original", &[("process.json", b"{\"present\":false}")]);
    let readonly = session(&provider, "session-readonly", "/audit/original", &[("readonly.json", b"{\"readonly\":true}")]);
    let retired_only = session(&provider, "session-retired-only", "/audit/original", &[]);
    fs::remove_file(retired_only.join("complete.json")).unwrap();
    let unsettled = session(&provider, "session-unsettled", "/audit/original", &[("producer-completion.json", b"{\"version\":1,\"audit_path\":\"/audit/original\",\"owner\":{\"workflow_id\":\"workflow-a\",\"lease_generation\":7},\"writer_fence\":7}"), ("upstream-export.json", b"{\"authenticated\":true,\"result\":\"retained\"}")]);
    let legacy = session(&provider, "session-legacy", "/audit/original", &[("legacy-retirement.json", b"{\"retired\":true}")]);
    let unrelated = session(&provider, "session-unrelated", "/audit/other", &[]);
    for (name, target) in [
        ("verified", "session-verified"), ("incomplete", "session-incomplete"), ("active", "session-active"),
        ("resumed", "session-resumed"), ("mismatch", "session-mismatch"), ("stale", "session-stale"),
        ("missing", "session-missing-process"), ("readonly", "session-readonly"), ("retired-only", "session-retired-only"),
        ("unsettled", "session-unsettled"), ("legacy", "session-legacy"), ("unrelated", "session-unrelated"),
    ] { alias(&aliases, name, target); }
    let preserved = root.join("workflow-preserved.json");
    let preserved_bytes = b"{\"request\":\"original\",\"acceptance\":\"original\",\"provenance\":\"original\",\"hold\":true,\"paused\":true,\"model_calls\":11,\"snapshot\":\"s\",\"rollback\":\"r\",\"budget\":\"b\"}";
    write(&preserved, preserved_bytes);
    let policy = root.join("policy.json");
    write(&policy, format!("{{\"version\":1,\"runtime_config\":\"/declared/runtime\",\"guest_runtime\":\"/declared/guest\",\"state_root\":\"{}\",\"memory_mb\":512,\"max_state_bytes\":2147483648,\"max_sessions\":64,\"providers\":{{}}}}", state.display()).as_bytes());
    std::env::set_var("AUGMENTAGENT_NATIVE_VM_POLICY", &policy);

    let held = OpenOptions::new().read(true).write(true).open(verified.join("lock")).unwrap();
    assert_eq!(unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
    augmentagent_channel_core::native_vm::reconcile_retired_runtimes().unwrap();
    let deferred = exists(&verified) && exists(&aliases.join("verified"));
    assert_eq!(unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_UN) }, 0);
    drop(held);
    augmentagent_channel_core::native_vm::reconcile_retired_runtimes().unwrap();

    let output = vec![
        !exists(&verified), !exists(&aliases.join("verified")), deferred,
        exists(&incomplete), exists(&aliases.join("incomplete")), exists(&active), exists(&resumed),
        exists(&mismatch), exists(&stale), exists(&missing), exists(&readonly), exists(&retired_only),
        exists(&unsettled), exists(&legacy), exists(&unrelated), exists(&aliases.join("unrelated")),
        fs::read(&preserved).unwrap() == preserved_bytes,
    ];
    println!("{}", serde_json::to_string(&output).unwrap());
    let _ = fs::remove_dir_all(&root);
}
