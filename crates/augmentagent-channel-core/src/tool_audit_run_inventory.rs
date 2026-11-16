//! Durable inventory of controller-registered supervised runs.
use super::AuditLogger;
use crate::native_vm::RuntimeOwner;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedRunInventory {
    pub inventory_complete: bool,
    pub registered_runs: usize,
    pub quiescent_runs: usize,
}
impl AuditLogger {
    /// Reads protected facts only. Counts are not an authorization to release
    /// custody or a substitute for fenced takeover and kernel scope proof.
    pub fn owned_run_inventory(&self, owner: &RuntimeOwner) -> anyhow::Result<OwnedRunInventory> {
        use anyhow::{ensure, Context};
        ensure!(
            self.is_protected() && owner.lease_generation > 0 && !owner.workflow_id.is_empty(),
            "owned inventory requires a protected audit and valid owner"
        );
        self.anchored
            .as_ref()
            .unwrap()
            .inspect_committed(|rows| inspect_rows(owner, rows))
    }
}
pub(super) fn inspect_rows(
    owner: &RuntimeOwner,
    rows: &[serde_json::Value],
) -> anyhow::Result<OwnedRunInventory> {
    use anyhow::{ensure, Context};

    let mut initialized = false;
    let mut complete = true;
    let mut current_owner: Option<RuntimeOwner> = None;
    let mut resources = 0_usize;
    let mut runs = std::collections::BTreeMap::<String, (std::path::PathBuf, Option<bool>)>::new();
    for row in rows {
        if row["actor"] != "controller" {
            continue;
        }
        match row["kind"].as_str().unwrap_or("") {
            "owned_supervisor_inventory_started" => {
                let actual: RuntimeOwner = serde_json::from_value(row["owner"].clone())?;
                initialized |= actual == *owner;
                current_owner = Some(actual);
            }
            "owned_supervisor_registered" if row["owner"] == serde_json::to_value(owner)? => {
                let id = row["run_id"]
                    .as_str()
                    .context("supervisor registration lacks identity")?;
                let receipt = std::path::PathBuf::from(
                    row["receipt"]
                        .as_str()
                        .context("supervisor registration lacks receipt")?,
                );
                ensure!(receipt.is_absolute(), "supervisor receipt is not absolute");
                ensure!(
                    runs.insert(id.into(), (receipt, None)).is_none(),
                    "duplicate supervisor registration"
                );
            }
            "owned_supervisor_completed" if row["owner"] == serde_json::to_value(owner)? => {
                let id = row["run_id"]
                    .as_str()
                    .context("supervisor completion lacks identity")?;
                if let Some((_, done)) = runs.get_mut(id) {
                    *done = Some(
                        row["clean"]
                            .as_bool()
                            .context("supervisor completion lacks cleanup status")?,
                    );
                } else {
                    complete = false;
                }
            }
            "run_resource_admitted" => {
                if current_owner.as_ref() == Some(owner) {
                    resources += 1;
                } else if current_owner.is_none() {
                    complete = false;
                }
            }
            "model_call_admitted" if current_owner.is_none() => complete = false,
            "model_call_admitted" if current_owner.as_ref() == Some(owner) && runs.is_empty() => {
                complete = false
            }
            _ => {}
        }
    }
    let mut quiet = 0;
    for (receipt, done) in runs.values() {
        match done {
            Some(true) => quiet += 1,
            Some(false) => complete = false,
            None => quiet += usize::from(cleanup_complete(receipt)?),
        }
    }
    Ok(OwnedRunInventory {
        inventory_complete: initialized && complete && resources <= runs.len(),
        registered_runs: runs.len(),
        quiescent_runs: quiet,
    })
}

fn cleanup_complete(path: &std::path::Path) -> anyhow::Result<bool> {
    use anyhow::ensure;
    use std::{
        io::Read,
        os::unix::fs::{MetadataExt, OpenOptionsExt},
    };
    let mut file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let meta = file.metadata()?;
    ensure!(
        meta.is_file()
            && meta.uid() == unsafe { libc::geteuid() }
            && meta.mode() & 0o077 == 0
            && meta.nlink() == 1
            && meta.len() <= 128,
        "supervisor cleanup receipt is not owner-private"
    );
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes == b"all-descendants-reaped\n" || bytes == b"cgroup-descendants-cleared\n")
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::{
        path::{Path, PathBuf},
        sync::Arc,
        time::Duration,
    };
    fn logger(root: &Path) -> (Arc<AuditLogger>, RuntimeOwner) {
        std::fs::create_dir_all(root.join("logs")).unwrap();
        std::fs::create_dir_all(root.join("anchors")).unwrap();
        let audit = Arc::new(
            AuditLogger::protected(
                root.join("logs/audit.jsonl"),
                root.join("anchors/anchor.json"),
                &[],
            )
            .unwrap(),
        );
        let owner = RuntimeOwner {
            workflow_id: "owner/repo#7".into(),
            lease_generation: 1,
        };
        audit.set_native_runtime_owner(owner.clone()).unwrap();
        (audit, owner)
    }
    async fn running(
        root: &Path,
        audit: &Arc<AuditLogger>,
    ) -> (
        tokio::process::Child,
        crate::run_resources::CommandGuard,
        i32,
    ) {
        let control = root.join("supervisor");
        std::fs::create_dir(&control).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&control, std::fs::Permissions::from_mode(0o700)).unwrap();
        let pidfile = root.join("grandchild.pid");
        let mut command = tokio::process::Command::new("/usr/bin/python3");
        command.args(["-I","-c","import os,sys,time; child=os.fork(); (os.setsid(),time.sleep(60)) if child==0 else (open(sys.argv[1],'w').write(str(child)),time.sleep(60))"]).arg(&pidfile);
        let (mut child, guard) =
            crate::run_resources::spawn_command_at(&command, Some(audit), &control).unwrap();
        guard.admit("fixture", Some(audit)).await.unwrap();
        drop(child.stdin.take());
        let pid = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(pid) = std::fs::read_to_string(&pidfile)
                    .ok()
                    .and_then(|raw| raw.parse::<i32>().ok())
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        (child, guard, pid)
    }
    #[test]
    fn fresh_empty_owned_inventory_is_complete_and_inspection_does_not_append_or_seal() {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let (audit, owner) = logger(root.path());
        audit
            .record_controller(serde_json::json!({"actor":"controller","kind":"fixture"}))
            .unwrap();
        let before = std::fs::read(audit.path()).unwrap();
        assert_eq!(
            audit.owned_run_inventory(&owner).unwrap(),
            OwnedRunInventory {
                inventory_complete: true,
                registered_runs: 0,
                quiescent_runs: 0
            }
        );
        assert_eq!(
            std::fs::read(audit.path()).unwrap(),
            before,
            "inspection appended an observation"
        );
        audit
            .record_controller(serde_json::json!({"actor":"controller","kind":"after_read"}))
            .unwrap();
    }
    #[tokio::test]
    async fn reopened_logger_sees_live_guard_then_actual_cleanup_without_using_memory() {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let (audit, owner) = logger(root.path());
        let (mut child, guard, pid) = running(root.path(), &audit).await;
        let (reopened, _) = logger(root.path());
        let live = reopened.owned_run_inventory(&owner).unwrap();
        assert!(
            live.inventory_complete,
            "registered supervisor inventory was unknown"
        );
        assert_eq!(
            (live.registered_runs, live.quiescent_runs),
            (1, 0),
            "a new logger lost the actual live supervisor"
        );
        assert!(
            reopened.recover_owned_runs(&owner, 2).is_err(),
            "live supervisor was sealed as quiescent"
        );
        audit
            .record_controller(serde_json::json!({"actor":"controller","kind":"still_open"}))
            .unwrap();
        drop(guard);
        let _ = child.wait().await;
        let quiet = reopened.owned_run_inventory(&owner).unwrap();
        assert_eq!((quiet.registered_runs, quiet.quiescent_runs), (1, 1));
        assert!(!Path::new(&format!("/proc/{pid}")).exists());
        assert!(
            reopened.recover_owned_runs(&owner, 1).is_err(),
            "same generation gained restart recovery authority"
        );
        let proof = reopened
            .recover_owned_runs(&owner, 2)
            .expect("dead supervised owner could not be recovered");
        assert_eq!(proof.owner(), &owner);
        assert_eq!(proof.runs(), 1);
        assert_eq!(proof.recovered_for_generation(), Some(2));
        let retry = AuditLogger::protected(
            root.path().join("logs/audit.jsonl"),
            root.path().join("anchors/anchor.json"),
            &[],
        )
        .unwrap();
        let recovered_again = retry.recover_owned_runs(&owner, 2).expect(
            "sealed restart proof could not be reopened without a live controller instance",
        );
        assert_eq!(recovered_again.recovered_for_generation(), Some(2));

        assert!(
            audit
                .record_controller(
                    serde_json::json!({"actor":"controller","kind":"late_old_append"})
                )
                .is_err(),
            "recovered old audit generation remained writable"
        );
    }
    #[tokio::test]
    async fn controller_exit_leaves_durable_supervisor_cleanup_inventory() {
        const CHILD: &str = "JARVIS_OWNED_INVENTORY_CHILD";
        if let Some(root) = std::env::var_os(CHILD) {
            let root = PathBuf::from(root);
            let (audit, _) = logger(&root);
            let (_child, _guard, _pid) = running(&root, &audit).await;
            std::process::exit(0); // Actual controller exit, bypassing all Rust destructors.
        }
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let result=std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact","tool_audit::run_inventory::tests::controller_exit_leaves_durable_supervisor_cleanup_inventory","--nocapture"])
            .env(CHILD,root.path()).output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let receipt = root.path().join("supervisor/cleanup-complete");
        tokio::time::timeout(Duration::from_secs(4), async {
            while !receipt.exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let (audit, owner) = logger(root.path());
        let inventory = audit.owned_run_inventory(&owner).unwrap();
        assert_eq!(
            inventory,
            OwnedRunInventory {
                inventory_complete: true,
                registered_runs: 1,
                quiescent_runs: 1
            },
            "restart could not recover controller-independent cleanup"
        );
        let pid = std::fs::read_to_string(root.path().join("grandchild.pid")).unwrap();
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "detached descendant survived controller exit"
        );
        let proof = audit
            .recover_owned_runs(&owner, 2)
            .expect("actual controller-exit custody could not be recovered");
        assert_eq!(proof.recovered_for_generation(), Some(2));
        std::fs::remove_dir_all(root.path().join("supervisor")).unwrap();
        let retry = AuditLogger::protected(
            root.path().join("logs/audit.jsonl"),
            root.path().join("anchors/anchor.json"),
            &[],
        )
        .unwrap();
        assert_eq!(
            retry
                .recover_owned_runs(&owner, 2)
                .expect("durable sealed proof depended on removed temporary receipts")
                .runs(),
            1
        );
    }
    #[test]
    fn untracked_resource_admission_is_unknown_and_corrupt_audit_is_an_error() {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let (audit, owner) = logger(root.path());
        audit.record_controller(serde_json::json!({"actor":"controller","kind":"run_resource_admitted","observation":{"identity":{}}})).unwrap();
        assert!(
            !audit
                .owned_run_inventory(&owner)
                .unwrap()
                .inventory_complete
        );
        let mut raw = std::fs::read(audit.path()).unwrap();
        raw[0] = b' ';
        std::fs::write(audit.path(), raw).unwrap();
        assert!(
            audit.owned_run_inventory(&owner).is_err(),
            "unanchored bytes were accepted as custody facts"
        );
    }
    #[tokio::test]
    async fn restart_proof_checks_kernel_identity_and_unresolved_writer_custody() {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let (audit, owner) = logger(root.path());
        let (mut child, guard, _) = running(root.path(), &audit).await;
        drop(guard);
        let _ = child.wait().await;
        audit.record_controller(serde_json::json!({"actor":"controller","kind":"run_resource_admitted","observation":{"identity":{"host_sha256":"different-host"}}})).unwrap();
        assert!(
            audit
                .owned_run_inventory(&owner)
                .unwrap()
                .inventory_complete
        );
        assert!(
            audit.recover_owned_runs(&owner, 2).is_err(),
            "a wrong-host scope became kernel quiescence proof"
        );
        audit.record_controller(serde_json::json!({"actor":"controller","kind":"still_open_after_bad_kernel_proof"})).unwrap();
        let other = tempfile::tempdir_in("/tmp").unwrap();
        let (audit, owner) = logger(other.path());
        let pending = audit.begin_write().unwrap();
        assert!(
            audit.recover_owned_runs(&owner, 2).is_err(),
            "live writer custody was sealed away"
        );
        pending.complete();
        assert!(audit.recover_owned_runs(&owner, 2).is_ok());
    }
    #[test]
    fn a_legacy_unscoped_model_admission_cannot_become_fresh_empty_inventory() {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        std::fs::create_dir(root.path().join("logs")).unwrap();
        std::fs::create_dir(root.path().join("anchors")).unwrap();
        let audit = AuditLogger::protected(
            root.path().join("logs/audit.jsonl"),
            root.path().join("anchors/anchor.json"),
            &[],
        )
        .unwrap();
        audit.record_controller(serde_json::json!({"actor":"controller","kind":"model_call_admitted","actual_provider":"codex"})).unwrap();
        let owner = RuntimeOwner {
            workflow_id: "owner/repo#7".into(),
            lease_generation: 1,
        };
        audit.set_native_runtime_owner(owner.clone()).unwrap();
        assert!(
            !audit
                .owned_run_inventory(&owner)
                .unwrap()
                .inventory_complete,
            "legacy unscoped admission became known-empty custody"
        );
        assert!(audit.recover_owned_runs(&owner, 2).is_err());
    }
    #[tokio::test]
    async fn supervisor_startup_is_registered_before_admission() {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let (audit, owner) = logger(root.path());
        let control = root.path().join("startup");
        std::fs::create_dir(&control).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&control, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut command = tokio::process::Command::new("/usr/bin/python3");
        command.args(["-I", "-c", "pass"]);
        let (mut child, guard) =
            crate::run_resources::spawn_command_at(&command, Some(&audit), &control).unwrap();
        let inventory = audit.owned_run_inventory(&owner).unwrap();
        assert_eq!(
            (inventory.registered_runs, inventory.quiescent_runs),
            (1, 0),
            "supervisor startup was reported as empty before admission"
        );
        assert!(audit.recover_owned_runs(&owner, 2).is_err());
        tokio::time::timeout(Duration::from_secs(2), async {
            while !control.join("launch-ready").exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        drop(guard);
        let _ = child.wait().await;
        assert_eq!(audit.owned_run_inventory(&owner).unwrap().quiescent_runs, 1);
    }
}
