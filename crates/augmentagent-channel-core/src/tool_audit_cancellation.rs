//! Controller-owned cancellation of supervised descendants. Model output is not proof.
use super::AuditLogger;
use crate::native_vm::RuntimeOwner;
use anyhow::{ensure, Context, Result};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

#[derive(Default)]
pub(super) struct Registry {
    state: Mutex<State>,
    changed: tokio::sync::Notify,
}
#[derive(Default)]
struct State {
    cancelled: bool,
    runs: Vec<Arc<Run>>,
}
struct Run {
    cancel: PathBuf,
    completed: Mutex<Option<bool>>,
}

/// Created only by an actual supervised guard; arbitrary audit rows cannot
/// register completion. Dropping without settlement makes recovery fail closed.
pub(crate) struct Registration {
    registry: Arc<Registry>,
    run: Arc<Run>,
    settled: bool,
    audit: AuditLogger,
    owner: RuntimeOwner,
    run_id: String,
}
impl Registration {
    pub(crate) fn complete(mut self, clean: bool) {
        let clean=clean && self.audit.record_controller(serde_json::json!({"actor":"controller",
            "kind":"owned_supervisor_completed","version":1,"owner":self.owner,"run_id":self.run_id,"clean":clean})).is_ok();
        if let Ok(mut slot) = self.run.completed.lock() {
            *slot = Some(clean);
        }
        self.settled = true;
        self.registry.changed.notify_waiters();
    }
}
impl Drop for Registration {
    fn drop(&mut self) {
        if !self.settled {
            if let Ok(mut slot) = self.run.completed.lock() {
                *slot = Some(false);
            }
            self.registry.changed.notify_waiters();
        }
    }
}
impl Registry {
    pub(crate) fn require_dispatch(&self) -> Result<()> {
        ensure!(
            !self
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("owned-run registry poisoned"))?
                .cancelled,
            "workflow cancellation requested; new dispatch is fenced"
        );
        Ok(())
    }
    fn register(
        self: &Arc<Self>,
        receipt: &Path,
        audit: &AuditLogger,
        owner: RuntimeOwner,
    ) -> Result<Registration> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("owned-run registry poisoned"))?;
        ensure!(
            !state.cancelled,
            "workflow cancellation requested; new supervisor is fenced"
        );
        let run = Arc::new(Run {
            cancel: receipt.with_file_name("cancel-requested"),
            completed: Mutex::new(None),
        });
        use sha2::{Digest, Sha256};
        let run_id = format!(
            "{:x}",
            Sha256::digest(receipt.as_os_str().as_encoded_bytes())
        );
        audit.record_controller(
            serde_json::json!({"actor":"controller","kind":"owned_supervisor_registered",
            "version":1,"owner":owner,"run_id":run_id,"receipt":receipt}),
        )?;
        state.runs.push(run.clone());
        Ok(Registration {
            registry: self.clone(),
            run,
            settled: false,
            audit: audit.clone(),
            owner,
            run_id,
        })
    }
    fn cancel(&self) -> Result<()> {
        use std::{io::Write, os::unix::fs::OpenOptionsExt};
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("owned-run registry poisoned"))?;
        state.cancelled = true;
        for run in &state.runs {
            if run
                .completed
                .lock()
                .map_err(|_| anyhow::anyhow!("owned run poisoned"))?
                .is_some()
            {
                continue;
            }
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&run.cancel)
            {
                Ok(mut file) => {
                    file.write_all(b"controller-cancelled\n")?;
                    file.sync_all()?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        self.changed.notify_waiters();
        Ok(())
    }
    async fn quiescent(&self) -> Result<usize> {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let state = self
                    .state
                    .lock()
                    .map_err(|_| anyhow::anyhow!("owned-run registry poisoned"))?;
                let statuses = state
                    .runs
                    .iter()
                    .map(|run| {
                        run.completed
                            .lock()
                            .map(|s| *s)
                            .map_err(|_| anyhow::anyhow!("owned run poisoned"))
                    })
                    .collect::<Result<Vec<_>>>()?;
                ensure!(
                    !statuses.contains(&Some(false)),
                    "owned descendant cleanup is unverified"
                );
                if state.cancelled && statuses.iter().all(Option::is_some) {
                    return Ok(statuses.len());
                }
            }
            changed.await;
        }
    }
}

#[derive(Debug)]
pub struct OwnedRunsQuiescence {
    owner: RuntimeOwner,
    audit_path: PathBuf,
    runs: usize,
    recovered_for_generation: Option<i64>,
}
impl OwnedRunsQuiescence {
    pub fn recovered_for_generation(&self) -> Option<i64> {
        self.recovered_for_generation
    }
    pub fn owner(&self) -> &RuntimeOwner {
        &self.owner
    }
    pub fn audit_path(&self) -> &Path {
        &self.audit_path
    }
    pub fn runs(&self) -> usize {
        self.runs
    }
}
impl AuditLogger {
    fn require_cancellation_owner(&self, owner: &RuntimeOwner) -> Result<()> {
        ensure!(
            self.is_protected() && self.native_runtime_owner()?.as_ref() == Some(owner),
            "cancellation requires the exact protected native owner"
        );
        Ok(())
    }
    pub(crate) fn register_owned_run(&self, receipt: &Path) -> Result<Option<Registration>> {
        if self.is_protected() {
            if let Some(owner) = self.native_runtime_owner()? {
                return Ok(Some(self.owned_runs.register(receipt, self, owner)?));
            }
        }
        Ok(None)
    }
    /// For a newer fenced controller only. Inspect the durable supervisor
    /// inventory and kernel scopes, then seal the old audit against new writers.
    /// The store must separately validate the successor lease before and after.
    pub fn recover_owned_runs(
        &self,
        owner: &RuntimeOwner,
        successor_generation: i64,
    ) -> Result<OwnedRunsQuiescence> {
        ensure!(
            self.is_protected() && owner.lease_generation > 0 && !owner.workflow_id.is_empty(),
            "restart custody requires a protected audit and valid owner"
        );
        ensure!(
            successor_generation > owner.lease_generation,
            "restart custody requires a newer fenced generation"
        );
        let sink = self.anchored.as_ref().unwrap();
        if let Some(sealed) = sink.sealed_custody_proof()? {
            let validation = &sealed["validation"];
            ensure!(
                validation["kind"] == "supervised_idle_recovery"
                    && validation["version"] == 1
                    && validation["owner"] == serde_json::to_value(owner)?,
                "sealed audit does not prove this supervised owner"
            );
            let runs = usize::try_from(
                validation["supervised_runs"]
                    .as_u64()
                    .context("sealed supervisor count missing")?,
            )?;
            return Ok(OwnedRunsQuiescence {
                owner: owner.clone(),
                audit_path: self.path.clone(),
                runs,
                recovered_for_generation: Some(successor_generation),
            });
        }
        // Refuse an incomplete committed prefix before attempting a seal.
        self.owned_run_inventory(owner)?;
        let mut runs = 0;
        self.anchored.as_ref().unwrap().seal_for_recovery(|rows| {
            ensure!(
                self.pending_writers
                    .load(std::sync::atomic::Ordering::Acquire)
                    == 0,
                "audit writers still active"
            );
            if let Some(dir) = &self.pending_dir {
                ensure!(
                    std::fs::read_dir(dir)?.next().is_none(),
                    "audit inventory has unresolved writer custody"
                );
            }
            let inventory = super::run_inventory::inspect_rows(owner, rows)?;
            ensure!(
                inventory.inventory_complete
                    && inventory.registered_runs == inventory.quiescent_runs,
                "old supervised custody is live or incomplete"
            );
            let mut resources = Vec::new();
            for row in rows {
                if row["actor"] == "controller" && row["kind"] == "run_resource_admitted" {
                    resources.push(crate::run_resources::verify_quiescent(
                        &row["observation"]["identity"],
                    )?);
                }
            }
            runs = inventory.registered_runs;
            Ok(
                serde_json::json!({"kind":"supervised_idle_recovery","version":1,"owner":owner,"successor_generation":successor_generation,
                "supervised_runs":runs,"quiescent_resources":resources}),
            )
        })?;
        Ok(OwnedRunsQuiescence {
            owner: owner.clone(),
            audit_path: self.path.clone(),
            runs,
            recovered_for_generation: Some(successor_generation),
        })
    }
    /// Request supervisor teardown, never signal an unrelated PID or release custody.
    pub fn cancel_owned_runs(&self, owner: &RuntimeOwner) -> Result<()> {
        self.require_cancellation_owner(owner)?;
        self.owned_runs.cancel()
    }
    /// A live guard or failed descendant cleanup cannot manufacture this proof.
    pub async fn owned_runs_quiescent(&self, owner: &RuntimeOwner) -> Result<OwnedRunsQuiescence> {
        self.require_cancellation_owner(owner)?;
        let runs = self.owned_runs.quiescent().await?;
        self.drain_checked().await?;
        Ok(OwnedRunsQuiescence {
            owner: owner.clone(),
            audit_path: self.path.clone(),
            runs,
            recovered_for_generation: None,
        })
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::{path::Path, sync::Arc, time::Duration};
    fn logger(root: &Path, name: &str) -> (Arc<AuditLogger>, RuntimeOwner) {
        std::fs::create_dir_all(root.join("logs")).unwrap();
        std::fs::create_dir_all(root.join("anchors")).unwrap();
        let logger = Arc::new(
            AuditLogger::protected(
                root.join("logs").join(format!("{name}.log")),
                root.join("anchors").join(format!("{name}.json")),
                &[],
            )
            .unwrap(),
        );
        let owner = RuntimeOwner {
            workflow_id: format!("owner/repo#{name}"),
            lease_generation: 1,
        };
        logger.set_native_runtime_owner(owner.clone()).unwrap();
        (logger, owner)
    }
    async fn running(
        root: &Path,
        logger: &Arc<AuditLogger>,
        name: &str,
    ) -> (
        tokio::process::Child,
        crate::run_resources::CommandGuard,
        i32,
    ) {
        let pidfile = root.join(format!("{name}.pid"));
        let mut command = tokio::process::Command::new("/usr/bin/python3");
        command.args(["-I","-c","import os,sys,time; child=os.fork(); (os.setsid(),time.sleep(60)) if child==0 else (open(sys.argv[1],'w').write(str(child)),time.sleep(60))"]).arg(&pidfile);
        let (mut child, guard) =
            crate::run_resources::spawn_command(&command, Some(logger)).unwrap();
        guard.admit("fixture", Some(logger)).await.unwrap();
        drop(child.stdin.take());
        let pid=tokio::time::timeout(Duration::from_secs(2),async {
            loop {
                if let Some(pid)=std::fs::read_to_string(&pidfile).ok().and_then(|raw|raw.parse::<i32>().ok()) {break pid;}
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }).await.unwrap();
        (child, guard, pid)
    }
    #[tokio::test]
    async fn cancellation_stops_detached_descendants_without_cancelling_another_owner() {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let (audit, owner) = logger(root.path(), "7");
        let (other, _) = logger(root.path(), "8");
        let (mut child, guard, pid) = running(root.path(), &audit, "first").await;
        let (mut peer, peer_guard, peer_pid) = running(root.path(), &other, "peer").await;
        audit.cancel_owned_runs(&owner).unwrap();
        let stopped = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
        assert!(
            stopped.is_ok(),
            "owner cancellation did not reach the live supervisor"
        );
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "detached descendant survived cancellation"
        );
        assert!(
            peer.try_wait().unwrap().is_none(),
            "another owner's supervisor was stopped"
        );
        assert!(Path::new(&format!("/proc/{peer_pid}")).exists());
        drop(guard);
        let proof =
            tokio::time::timeout(Duration::from_secs(2), audit.owned_runs_quiescent(&owner))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(proof.owner(), &owner);
        assert_eq!(proof.runs(), 1);
        drop(peer_guard);
        let _ = peer.wait().await;
    }
    #[tokio::test]
    async fn live_guard_cannot_be_reported_quiescent_and_cancelled_owner_cannot_dispatch() {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let (audit, owner) = logger(root.path(), "7");
        let (mut child, guard, _) = running(root.path(), &audit, "live").await;
        assert!(
            tokio::time::timeout(
                Duration::from_millis(30),
                audit.owned_runs_quiescent(&owner)
            )
            .await
            .is_err(),
            "a live guard was reported quiescent"
        );
        audit.cancel_owned_runs(&owner).unwrap();
        assert!(
            audit.dispatch_allowed().is_err(),
            "cancelled owner could dispatch again"
        );
        drop(guard);
        let _ = child.wait().await;
    }
    #[test]
    fn cancellation_requires_the_exact_protected_native_owner() {
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let (audit, mut owner) = logger(root.path(), "7");
        owner.lease_generation += 1;
        assert!(
            audit.cancel_owned_runs(&owner).is_err(),
            "different lease cancelled this owner"
        );
        let unprotected = AuditLogger::new(root.path().join("unprotected"));
        assert!(
            unprotected.cancel_owned_runs(&owner).is_err(),
            "unprotected logger acquired cancellation authority"
        );
    }
}
