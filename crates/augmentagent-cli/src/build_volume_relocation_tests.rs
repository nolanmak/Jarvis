//! Exercise the command handler, ownership registry, leases, real pruner and
//! admission report. Only the OS filesystem boundary is injected: fixtures
//! must never relocate the production volumes or require a second large disk.
use super::*;
use crate::build_relocation::{self, Filesystem, Probe};

struct Volumes {
    new: PathBuf,
    mode: String,
}
impl Probe for Volumes {
    fn inspect(&self, path: &Path) -> Result<Filesystem> {
        let dest = path.starts_with(&self.new);
        Ok(Filesystem {
            uid: if dest && self.mode == "owner" {
                unsafe { libc::geteuid() }.wrapping_add(1)
            } else {
                unsafe { libc::geteuid() }
            },
            device: if dest && self.mode != "same" { 2 } else { 1 },
            free: if dest && self.mode == "headroom" {
                1
            } else {
                100 * 1024u64.pow(3)
            },
            writable: self.mode != "writable" || !dest,
        })
    }
}
fn snapshot(root: &Path) -> Vec<(PathBuf, u32, u64, Vec<u8>)> {
    fn visit(base: &Path, path: &Path, entries: &mut Vec<(PathBuf, u32, u64, Vec<u8>)>) {
        let meta = std::fs::symlink_metadata(path).unwrap();
        entries.push((
            path.strip_prefix(base).unwrap().to_path_buf(),
            meta.mode(),
            meta.mtime() as u64,
            if meta.is_file() {
                std::fs::read(path).unwrap()
            } else {
                Vec::new()
            },
        ));
        if meta.is_dir() {
            for entry in std::fs::read_dir(path).unwrap() {
                visit(base, &entry.unwrap().path(), entries);
            }
        }
    }
    let mut entries = Vec::new();
    visit(root, root, &mut entries);
    entries.sort();
    entries
}

fn fixture_cache(repo: &Path, state: &Path, path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    register(repo, path, None, state).unwrap();
    std::fs::create_dir_all(path.join("debug/deps")).unwrap();
    std::fs::write(path.join("debug/deps/warm.rlib"), vec![7; 4096]).unwrap();
    register(repo, path, None, state).unwrap();
}
#[test]
fn build_volume_command_fixture_child() {
    let Some(mode) = std::env::var_os("RELOCATION_CASE") else {
        return;
    };
    let mode = mode.to_string_lossy().into_owned();
    let root = PathBuf::from(std::env::var_os("RELOCATION_ROOT").unwrap());
    let repo = root.join("repo");
    std::fs::create_dir(&repo).unwrap();
    assert!(std::process::Command::new("git")
        .args(["init", "-q"])
        .arg(&repo)
        .status()
        .unwrap()
        .success());
    let old = root.join("old");
    let volume = root.join("volume");
    std::fs::create_dir(&volume).unwrap();
    std::fs::create_dir(&old).unwrap();
    let state = state_dir().unwrap();
    let parent = old.join("sessions");
    manage_parent(&repo, &parent, &state).unwrap();
    let idle = parent.join("idle");
    let busy = parent.join("busy");
    fixture_cache(&repo, &state, &idle);
    fixture_cache(&repo, &state, &busy);
    let foreign = old.join("unregistered/debug/precious");
    std::fs::create_dir_all(foreign.parent().unwrap()).unwrap();
    std::fs::write(&foreign, b"foreign").unwrap();
    let busy_before = snapshot(&busy);
    let _busy = crate::storage_leases::TargetLeases::try_directory(&busy)
        .unwrap()
        .unwrap();
    let probe = Volumes {
        new: volume.clone(),
        mode: mode.clone(),
    };
    let to = if mode == "symlink" {
        let link = root.join("link");
        std::os::unix::fs::symlink(&volume, &link).unwrap();
        link
    } else {
        volume.clone()
    };
    let before = snapshot(&root);
    let mut output = Vec::new();
    let result = if mode == "native-same" {
        unsafe {
            std::env::set_var("AUGMENTAGENT_BUILD_VOLUME", &old);
        }
        build_relocation::run(&repo, &to, true, &mut output)
    } else {
        build_relocation::run_with(&repo, &to, mode != "apply", &mut output, &probe)
    };
    if let Err(error) = result {
        assert_eq!(snapshot(&root), before, "refusal mutated files");
        eprintln!("{error:#}");
        std::process::exit(1);
    }
    let plan: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert!(plan["configuration"]
        .as_str()
        .unwrap()
        .starts_with("AUGMENTAGENT_BUILD_VOLUME="));
    assert_eq!(plan["volume"], serde_json::to_value(&volume).unwrap());
    assert!(plan["retire"]
        .as_array()
        .unwrap()
        .contains(&serde_json::to_value(&idle).unwrap()));
    if mode == "dry" {
        assert_eq!(snapshot(&root), before, "dry-run mutated files");
        print!("{}", String::from_utf8(output).unwrap());
        return;
    }
    assert_eq!(mode, "apply");
    assert!(volume
        .join("augmentagent-session-targets/.augmentagent-build-parent.json")
        .exists());
    assert!(volume
        .join("augmentagent-verifier-targets/.augmentagent-build-parent.json")
        .exists());
    assert!(volume
        .join("augmentagent-gate-target/.augmentagent-target.json")
        .exists());
    assert_eq!(snapshot(&busy), busy_before, "relocate touched busy cache");
    assert!(
        idle.join("debug/deps/warm.rlib").exists(),
        "relocate copied/deleted artifacts before prune"
    );
    // Applying the printed setting is the operator's step, not a command-side
    // mutation of .env or the running worker.
    unsafe {
        std::env::set_var("AUGMENTAGENT_BUILD_VOLUME", &volume);
    }
    let mut prune = Vec::new();
    crate::disk::run_prune(
        &repo,
        &crate::disk::Options {
            json: true,
            ..Default::default()
        },
        &mut prune,
    )
    .unwrap();
    assert!(
        !idle.join("debug").exists(),
        "{}",
        String::from_utf8_lossy(&prune)
    );
    assert_eq!(snapshot(&busy), busy_before, "prune removed leased cache");
    assert_eq!(std::fs::read(foreign).unwrap(), b"foreign");
    assert!(!snapshot(&volume)
        .iter()
        .any(|e| e.0.file_name().is_some_and(|n| n == "warm.rlib")));
    // Same admission machinery as a managed build: the old disk has no space,
    // but every actual output on V has 100 GiB. Reverse that to prove the
    // other volume's free bytes cannot satisfy this target's budget.
    let target = effective_target(&repo);
    let free = |p: &Path| {
        Some(if p.starts_with(&volume) {
            100 * 1024u64.pow(3)
        } else {
            0
        })
    };
    reconcile(
        &repo,
        &target,
        &state,
        admission_floor_gb(),
        cache_cap_mb(),
        None,
        &free,
    )
    .unwrap();
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(state.join("disk-reconcile.json")).unwrap()).unwrap();
    assert_eq!(report["admitted"], true);
    for reading in report["filesystem_free_bytes"].as_array().unwrap() {
        assert!(Path::new(reading[0].as_str().unwrap()).starts_with(&volume));
        assert_eq!(reading[1], 100 * 1024u64.pow(3));
    }
    assert!(reconcile(
        &repo,
        &target,
        &state,
        admission_floor_gb(),
        cache_cap_mb(),
        None,
        &|p| Some(if p.starts_with(&volume) {
            0
        } else {
            100 * 1024u64.pow(3)
        })
    )
    .is_err());
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(state.join("disk-reconcile.json")).unwrap()).unwrap();
    assert_eq!(report["admitted"], false);
    let status = crate::disk::read_filesystems(&repo);
    for role in [
        "gate output",
        "session output parent",
        "verifier output parent",
    ] {
        assert!(status
            .iter()
            .flat_map(|f| &f.labels)
            .any(|label| label.starts_with(role) && label.contains(volume.to_str().unwrap())));
    }
    unsafe {
        std::env::set_var("CARGO_TARGET_DIR", &idle);
    }
    let mut explicit_opts = crate::disk::Options::default();
    build_relocation::retired_options(
        &repo,
        &crate::disk::Root::at("explicit", &idle),
        &mut explicit_opts,
    )
    .unwrap();
    assert_eq!(explicit_opts.cap_mb, None);
    unsafe {
        std::env::remove_var("CARGO_TARGET_DIR");
    }
    // Returning to old configuration protects the active caches even though
    // their retirement intentions remain available for the next move.
    unsafe {
        std::env::set_var("AUGMENTAGENT_BUILD_PARENT", &parent);
    }
    let mut opts = crate::disk::Options::default();
    build_relocation::retired_options(&repo, &crate::disk::Root::at("old", &idle), &mut opts)
        .unwrap();
    assert_eq!(opts.cap_mb, None);
}
fn run_case(mode: &str, reason: Option<&str>) {
    let root = tempfile::tempdir().unwrap();
    for name in ["home", "tmp"] {
        std::fs::create_dir(root.path().join(name)).unwrap();
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "build_storage::build_volume_relocation_tests::build_volume_command_fixture_child",
            "--nocapture",
        ])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", root.path().join("home"))
        .env("XDG_STATE_HOME", root.path().join("state-home"))
        .env("TMPDIR", root.path().join("tmp"))
        .env("RELOCATION_ROOT", root.path())
        .env("RELOCATION_CASE", mode)
        .output()
        .unwrap();
    let all = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if let Some(reason) = reason {
        assert_eq!(output.status.code(), Some(1), "{all}");
        assert!(all.contains(reason), "{all}");
    } else {
        assert!(output.status.success(), "{all}");
        if mode == "dry" {
            assert!(all.contains("AUGMENTAGENT_BUILD_VOLUME="));
        }
    }
}
#[test]
fn build_volume_dry_run_is_read_only_and_prints_configuration() {
    run_case("dry", None);
}
#[test]
fn build_volume_refuses_non_owned_destination() {
    run_case("owner", Some("relocate_destination_not_owned"));
}
#[test]
fn build_volume_refuses_non_writable_destination() {
    run_case("writable", Some("relocate_destination_not_writable"));
}
#[test]
fn build_volume_refuses_symlink_destination() {
    run_case("symlink", Some("relocate_destination_symlink"));
}
#[test]
fn build_volume_refuses_same_filesystem() {
    run_case("same", Some("relocate_same_filesystem"));
}
#[test]
fn build_volume_refuses_insufficient_headroom() {
    run_case("headroom", Some("relocate_insufficient_headroom"));
}
#[test]
fn build_volume_relocation_prunes_idle_preserves_busy_and_admits_on_destination() {
    run_case("apply", None);
}

#[test]
fn build_volume_native_probe_refuses_actual_same_filesystem() {
    run_case("native-same", Some("relocate_same_filesystem"));
}
