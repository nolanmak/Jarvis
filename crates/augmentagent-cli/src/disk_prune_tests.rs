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
