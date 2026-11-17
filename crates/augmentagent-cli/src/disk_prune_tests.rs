//! Public pruning path with real ownership records, files, and process leases.
use super::*;
use std::collections::BTreeMap;
use std::process::{Child, Command};

struct Fixture {
    _tmp: tempfile::TempDir,
    repo: PathBuf,
    state: PathBuf,
    a: PathBuf,
    b: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let state = tmp.path().join("state");
        assert!(Command::new("git").args(["init", "-q"]).arg(&repo).status().unwrap().success());
        let a = tmp.path().join("target-a");
        let b = tmp.path().join("target-b");
        for target in [&a, &b] {
            crate::build_storage::register(&repo, target, None, &state).unwrap();
            let session = target.join("debug/incremental/old-session");
            std::fs::create_dir_all(&session).unwrap();
            let artifact = session.join("cache");
            std::fs::write(&artifact, vec![7; 8192]).unwrap();
            let old = SystemTime::now() - Duration::from_secs(4 * 86400);
            for path in [&artifact, &session] {
                std::fs::File::open(path).unwrap().set_times(std::fs::FileTimes::new().set_modified(old)).unwrap();
            }
            std::fs::write(target.join("debug/augmentagent"), b"executable sentinel").unwrap();
            std::fs::write(target.join("unknown"), b"unknown sentinel").unwrap();
        }
        Self { _tmp: tmp, repo, state, a, b }
    }
    fn prune(&self, roots: Vec<PathBuf>, dry_run: bool, cap_mb: Option<u64>, procs: Option<Vec<BuildProc>>) -> serde_json::Value {
        let mut out = Vec::new();
        run_prune_at(&self.repo, &Options { roots, dry_run, cap_mb, json: true, ..Default::default() },
            &mut out, &self.state, || procs).unwrap();
        serde_json::from_slice(&out).unwrap()
    }
}

fn tree(path: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(base: &Path, path: &Path, entries: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            let meta = path.symlink_metadata().unwrap();
            let value = if meta.is_dir() { b"directory".to_vec() }
                else if meta.file_type().is_symlink() { std::fs::read_link(&path).unwrap().as_os_str().as_encoded_bytes().to_vec() }
                else { std::fs::read(&path).unwrap() };
            entries.insert(path.strip_prefix(base).unwrap().to_path_buf(), value);
            if meta.is_dir() { walk(base, &path, entries); }
        }
    }
    let mut entries = BTreeMap::new();
    walk(path, path, &mut entries);
    entries
}

struct ChildLease(Child);
impl Drop for ChildLease {
    fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); }
}
fn child_lease(target: &Path) -> ChildLease {
    let ready = target.parent().unwrap().join("child-ready");
    let child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "disk::prune_tests::target_lease_child", "--ignored"])
        .env("JARVIS_PRUNE_TEST_TARGET", target).env("JARVIS_PRUNE_TEST_READY", &ready)
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
    let mut child = ChildLease(child);
    let until = std::time::Instant::now() + Duration::from_secs(10);
    while !ready.exists() {
        assert!(child.0.try_wait().unwrap().is_none(), "target lease child exited");
        assert!(std::time::Instant::now() < until, "target lease child timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
    child
}

#[test]
#[ignore = "subprocess helper invoked by the parent regression"]
fn target_lease_child() {
    let target = PathBuf::from(std::env::var_os("JARVIS_PRUNE_TEST_TARGET").unwrap());
    let _lease = crate::storage_leases::TargetLeases::try_cache_roots(&[target], None, &profile_dirs).unwrap().unwrap();
    std::fs::write(std::env::var_os("JARVIS_PRUNE_TEST_READY").unwrap(), b"ready").unwrap();
    std::thread::sleep(Duration::from_secs(30));
}

#[test]
fn global_writer_does_not_starve_idle_owned_target() {
    let f = Fixture::new();
    let _global = crate::build_storage::Lease::try_at(&f.state).unwrap().unwrap();
    let mut writer = child_lease(&f.a);
    let before = tree(&f.a);
    let reports = f.prune(vec![f.a.clone(), f.b.clone()], false, None, Some(vec![]));
    assert!(!f.b.join("debug/incremental/old-session").exists(), "idle target starved: {reports}");
    assert_eq!(tree(&f.a), before, "busy target changed");
    assert!(writer.0.try_wait().unwrap().is_none());
    assert!(crate::build_storage::Lease::try_at(&f.state).unwrap().is_none());
    let reports = reports.as_array().unwrap();
    let b = reports.iter().find(|r| r["path"] == f.b.display().to_string()).unwrap();
    assert!(b["freed_bytes"].as_u64().unwrap() > 0);
    assert_eq!(b["outcome"], "pruned");
    assert_eq!(std::fs::read(f.b.join("debug/augmentagent")).unwrap(), b"executable sentinel");
    assert_eq!(std::fs::read(f.b.join("unknown")).unwrap(), b"unknown sentinel");
    assert!(reports.iter().any(|r| r["outcome"] == "deferred"));
}

#[test]
fn global_writer_allows_cap_dry_run_with_identical_tree() {
    let f = Fixture::new();
    let _global = crate::build_storage::Lease::try_at(&f.state).unwrap().unwrap();
    let before = tree(&f.b);
    let reports = f.prune(vec![f.b.clone()], true, Some(0), Some(vec![]));
    assert!(reports[0]["freed_bytes"].as_u64().unwrap() > 0, "dry run starved: {reports}");
    assert_eq!(reports[0]["path"], f.b.display().to_string());
    assert_eq!(tree(&f.b), before);
}

#[test]
fn global_writer_reports_other_process_target_as_deferred() {
    let f = Fixture::new();
    let _global = crate::build_storage::Lease::try_at(&f.state).unwrap().unwrap();
    let mut writer = child_lease(&f.a);
    let before = tree(&f.a);
    let reports = f.prune(vec![f.a.clone()], false, Some(0), Some(vec![]));
    assert_eq!(reports[0]["path"], f.a.display().to_string(), "must name deferred target: {reports}");
    assert_eq!(reports[0]["outcome"], "deferred");
    assert_eq!(reports[0]["removed"], 0);
    assert_eq!(tree(&f.a), before);
    assert!(writer.0.try_wait().unwrap().is_none());
}

#[test]
fn global_writer_still_requires_compiler_evidence() {
    let f = Fixture::new();
    let _global = crate::build_storage::Lease::try_at(&f.state).unwrap().unwrap();
    let before = tree(&f.b);
    f.prune(vec![f.b.clone()], false, Some(0), None);
    assert_eq!(tree(&f.b), before);
    f.prune(vec![f.b.clone()], false, Some(0), Some(vec![BuildProc { target_env: Some(f.b.clone()), ..Default::default() }]));
    assert_eq!(tree(&f.b), before);
}

#[test]
fn global_writer_preserves_whole_root_retirement() {
    isolated_retention_test("disk::prune_tests::whole_root_retirement_child");
}

fn isolated_retention_test(test_name: &str) {
    let isolated = tempfile::tempdir().unwrap();
    let scratch = isolated.path().join("scratch");
    std::fs::create_dir(&scratch).unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--ignored", "--nocapture"])
        .env("XDG_STATE_HOME", isolated.path().join("state"))
        .env("TMPDIR", &scratch)
        .env("AUGMENTAGENT_GATE_TARGET_DIR", isolated.path().join("gate"))
        .env("AUGMENTAGENT_PRUNE_TARGET_DIRS", "")
        .env("AUGMENTAGENT_PRUNE_VERIFIER_KEEP", "3")
        .env("AUGMENTAGENT_PRUNE_VERIFIER_MAX_GB", "20")
        .env("AUGMENTAGENT_GATE_MIN_FREE_GB", "0")
        .env("AUGMENTAGENT_BUILD_GROWTH_GB", "0")
        .output().unwrap();
    assert!(output.status.success(), "{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
}

#[test]
#[ignore = "isolated environment helper invoked by the parent regression"]
fn whole_root_retirement_child() {
    let mut f = Fixture::new();
    // Discover through actual registration records in the child's isolated
    // state directory, keeping the parent's process environment untouched.
    f.state = crate::build_storage::state_dir().unwrap();
    crate::build_storage::register(&f.repo, &f.a, None, &f.state).unwrap();
    crate::build_storage::register_abandoned(&f.repo, &f.b, &f.state).unwrap();
    fn age_tree(path: &Path) {
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() { age_tree(&path); }
            else { age(&path); }
        }
        age(path);
    }
    fn age(path: &Path) {
        let old = SystemTime::now() - Duration::from_secs(5 * 86400);
        std::fs::File::open(path).unwrap().set_times(std::fs::FileTimes::new().set_modified(old)).unwrap();
    }
    age_tree(&f.b);
    let _global = crate::build_storage::Lease::try_at(&f.state).unwrap().unwrap();
    let mut writer = child_lease(&f.a);
    let before_a = tree(&f.a);
    let before_b = tree(&f.b);
    for dry_run in [true, false] {
        let reports = f.prune(vec![], dry_run, None, Some(vec![]));
        let report = reports.as_array().unwrap().iter()
            .find(|r| r["path"] == f.b.display().to_string()).expect("owned orphan was not discovered");
        assert_eq!(report["full_drop"], true, "whole-root selection lost: {reports}");
        assert_eq!(report["outcome"], "pruned");
        assert!(report["freed_bytes"].as_u64().unwrap() > 0);
        if dry_run { assert_eq!(tree(&f.b), before_b); }
        else { assert!(!f.b.exists(), "whole orphan target must be removed"); }
        assert_eq!(tree(&f.a), before_a);
        assert!(writer.0.try_wait().unwrap().is_none());
        assert!(crate::build_storage::Lease::try_at(&f.state).unwrap().is_none());
    }
}


#[test]
fn global_writer_runs_verifier_retention_and_defers_busy_verifier() {
    isolated_retention_test("disk::prune_tests::verifier_retention_child");
}

#[test]
#[ignore = "isolated environment helper invoked by the parent regression"]
fn verifier_retention_child() {
    let mut f = Fixture::new();
    f.state = crate::build_storage::state_dir().unwrap();
    fn age_tree(path: &Path, time: SystemTime) {
        if path.is_dir() {
            for entry in std::fs::read_dir(path).unwrap() {
                age_tree(&entry.unwrap().path(), time);
            }
        }
        std::fs::File::open(path).unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(time)).unwrap();
    }
    let caches: Vec<_> = (0..5).map(|i| {
        let path = f._tmp.path().join(format!("augmentagent-verifier-{i:064x}"));
        crate::build_storage::register(&f.repo, &path, Some(&f.repo), &f.state).unwrap();
        std::fs::create_dir_all(path.join("debug/deps")).unwrap();
        std::fs::write(path.join("debug/deps/warm.rlib"), vec![7; 8192]).unwrap();
        age_tree(&path, SystemTime::now() - Duration::from_secs(3500 - i * 300));
        path
    }).collect();
    let _global = crate::build_storage::Lease::try_at(&f.state).unwrap().unwrap();
    let mut writer = child_lease(&caches[0]);
    let before: Vec<_> = caches.iter().map(|p| tree(p)).collect();
    for dry in [true, false] {
        let reports = f.prune(vec![], dry, None, Some(vec![]));
        let reports = reports.as_array().unwrap();
        let busy = reports.iter().find(|r| r["path"] == caches[0].display().to_string())
            .expect("leased verifier must be named");
        assert_eq!(busy["outcome"], "deferred", "busy verifier must not report pruned: {busy}");
        assert_eq!(busy["removed"], 0);
        for i in [1, 2] {
            let retired = reports.iter().find(|r| r["path"] == caches[i].display().to_string())
                .expect("verifier retention must run inside the busy-global path");
            assert_eq!(retired["outcome"], "pruned");
            assert_eq!(retired["full_drop"], true);
            assert!(retired["freed_bytes"].as_u64().unwrap() > 0);
        }
        for (i, path) in caches.iter().enumerate() {
            if dry || i == 0 || i >= 3 { assert_eq!(tree(path), before[i]); }
            else { assert!(!path.exists(), "older idle verifier must be retired regardless of age"); }
        }
        assert!(writer.0.try_wait().unwrap().is_none());
        assert!(crate::build_storage::Lease::try_at(&f.state).unwrap().is_none());
    }
}
