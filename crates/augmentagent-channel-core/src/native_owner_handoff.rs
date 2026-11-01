//! Guarded handoff orchestration. No provider is launched by this module.
use super::owner_ledger::{Endpoint, Ledger, State};
use super::ownership::TransferReason as R;
use super::ownership::{
    canonical_bytes, Authority, RuntimeInventoryRow, SealReference, TransferReceipt,
    TransferRequest,
};
use crate::tool_audit::AuditLogger;
use anyhow::{ensure, Context, Result};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    os::fd::AsRawFd,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::Arc,
};
fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn value_hash(value: &serde_json::Value) -> Result<String> {
    Ok(sha(&canonical_bytes(value)?))
}
fn private_directory(path: &Path) -> Result<()> {
    let info = path.symlink_metadata()?;
    ensure!(
        info.is_dir()
            && !info.file_type().is_symlink()
            && info.uid() == unsafe { libc::geteuid() }
            && info.mode() & 0o077 == 0,
        R::MetadataInvalid
    );
    Ok(())
}
fn actual_audit(path: &Path) -> Result<PathBuf> {
    let info = path.symlink_metadata()?;
    ensure!(
        info.is_file()
            && info.uid() == unsafe { libc::geteuid() }
            && info.mode() & 0o077 == 0
            && info.nlink() == 1,
        R::AuditCorrupt
    );
    Ok(path.canonicalize()?)
}
struct Source<'a> {
    logger: &'a Arc<AuditLogger>,
    references: Vec<String>,
}
struct Audits<'a> {
    sources: Vec<Source<'a>>,
    endpoints: Vec<Endpoint>,
}
fn audit_plan<'a>(
    request: &TransferRequest,
    sources: &'a [Arc<AuditLogger>],
    destination: &Arc<AuditLogger>,
) -> Result<Audits<'a>> {
    let mut expected: BTreeMap<PathBuf, Vec<&Authority>> = BTreeMap::new();
    for authority in &request.source_owners {
        expected
            .entry(actual_audit(Path::new(&authority.audit_path))?)
            .or_default()
            .push(authority);
    }
    let mut supplied = BTreeMap::new();
    for logger in sources {
        let log = logger.protected_log_path().context(R::AuditCorrupt)?;
        ensure!(
            actual_audit(logger.path())? == log
                && supplied.insert(log.to_path_buf(), logger).is_none(),
            R::AuditCorrupt
        );
    }
    ensure!(supplied.keys().eq(expected.keys()), R::AuditCorrupt);
    let destination_log = actual_audit(Path::new(&request.destination.audit_path))?;
    ensure!(
        Some(destination_log.as_path()) == destination.protected_log_path()
            && actual_audit(destination.path())? == destination_log
            && !expected.contains_key(&destination_log),
        R::AuthorityConflict
    );
    let destination_anchor = actual_audit(
        destination
            .protected_anchor_path()
            .context(R::AuditCorrupt)?,
    )?;
    let mut endpoints = vec![Endpoint {
        audit_path: request.destination.audit_path.clone(),
        actual_audit_path: destination_log.to_str().context(R::AuditCorrupt)?.into(),
        anchor_path: destination_anchor.to_str().context(R::AuditCorrupt)?.into(),
    }];
    let mut planned = Vec::new();
    for (path, owners) in expected {
        let logger = supplied[&path];
        let highest = owners
            .iter()
            .max_by_key(|owner| owner.owner.lease_generation)
            .unwrap();
        ensure!(
            logger.native_runtime_owner()?.as_ref() == Some(&highest.owner),
            R::AuthorityConflict
        );
        let anchor = actual_audit(logger.protected_anchor_path().context(R::AuditCorrupt)?)?;
        ensure!(anchor != destination_anchor, R::AuthorityConflict);
        let mut references = Vec::new();
        for owner in owners {
            endpoints.push(Endpoint {
                audit_path: owner.audit_path.clone(),
                actual_audit_path: path.to_str().context(R::AuditCorrupt)?.into(),
                anchor_path: anchor.to_str().context(R::AuditCorrupt)?.into(),
            });
            references.push(owner.audit_path.clone());
        }
        references.sort();
        references.dedup();
        planned.push(Source { logger, references });
    }
    Ok(Audits {
        sources: planned,
        endpoints,
    })
}
struct SessionLock(std::fs::File);
impl Drop for SessionLock {
    fn drop(&mut self) {
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}
fn session_lock(path: &Path) -> Result<SessionLock> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path.join("lock"))?;
    let info = file.metadata()?;
    ensure!(
        info.is_file()
            && info.uid() == unsafe { libc::geteuid() }
            && info.mode() & 0o077 == 0
            && info.nlink() == 1,
        R::MetadataInvalid
    );
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        anyhow::bail!(R::LockBusy)
    }
    Ok(SessionLock(file))
}
fn retired(session: &Path) -> Result<bool> {
    let mut retired = false;
    for name in ["retired.json", "retired-owner.json"] {
        match session.join(name).symlink_metadata() {
            Ok(_) => {
                let marker: serde_json::Value =
                    serde_json::from_slice(&super::private(&session.join(name))?)?;
                ensure!(marker["version"] == 1, R::MetadataInvalid);
                retired = true;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(retired)
}
fn inventory_paths(
    root: &Path,
    ledger: &Ledger<'_>,
    request: &TransferRequest,
) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for provider in ["codex", "claude", "qwen", "glm"] {
        let directory = root.join(provider);
        match directory.symlink_metadata() {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
            Ok(_) => private_directory(&directory)?,
        }
        for entry in std::fs::read_dir(&directory)? {
            let path = entry?.path();
            if !path
                .file_name()
                .context(R::MetadataInvalid)?
                .to_string_lossy()
                .starts_with("session-")
            {
                continue;
            }
            private_directory(&path)?;
            let bytes = super::private(&path.join("owned.json"))?;
            let owned: serde_json::Value = serde_json::from_slice(&bytes)?;
            ensure!(owned["version"] == 1, R::MetadataInvalid);
            let creation: super::RuntimeOwner = serde_json::from_value(owned["owner"].clone())?;
            ensure!(
                !creation.workflow_id.is_empty() && creation.lease_generation > 0,
                R::MetadataInvalid
            );
            if creation.workflow_id != request.workflow_id {
                continue;
            }
            if retired(&path)? {
                continue;
            }
            ensure!(
                matches!(provider, "codex" | "claude"),
                R::RuntimeUnavailable
            );
            let audit_path = owned["audit_path"]
                .as_str()
                .context(R::MetadataInvalid)?
                .into();
            let effective = ledger.resolve(
                super::relative_runtime(root, &path)?,
                &Authority {
                    owner: creation,
                    audit_path,
                },
                &sha(&bytes),
            )?;
            ensure!(
                !ledger.is_closed(&effective.owner)? && !ledger.held_by_abort(&effective.owner)?,
                R::AuthorityConflict
            );
            ensure!(
                request.source_owners.contains(&effective),
                R::InventoryMissing
            );
            paths.push(path);
        }
    }
    paths.sort();
    let declared = request
        .inventory
        .iter()
        .map(|row| root.join(&row.runtime_relative_path))
        .collect::<Vec<_>>();
    let mut declared_sorted = declared.clone();
    declared_sorted.sort();
    ensure!(paths == declared_sorted, R::InventoryMissing);
    Ok(paths)
}
fn exact_inventory_row(
    root: &Path,
    ledger: &Ledger<'_>,
    declared: &RuntimeInventoryRow,
) -> Result<()> {
    let session = root.join(&declared.runtime_relative_path);
    private_directory(&session)?;
    ensure!(!retired(&session)?, R::AuthorityConflict);
    let owned = super::private(&session.join("owned.json"))?;
    let creation: serde_json::Value = serde_json::from_slice(&owned)?;
    let creation_owner: super::RuntimeOwner = serde_json::from_value(creation["owner"].clone())?;
    let creation_audit_path = creation["audit_path"]
        .as_str()
        .context(R::MetadataInvalid)?
        .to_string();
    let effective = ledger.resolve(
        &declared.runtime_relative_path,
        &Authority {
            owner: creation_owner.clone(),
            audit_path: creation_audit_path.clone(),
        },
        &sha(&owned),
    )?;
    let complete = super::private(&session.join("complete.json")).context(R::RuntimeUnavailable)?;
    let completion: serde_json::Value = serde_json::from_slice(&complete)?;
    ensure!(
        completion["version"] == 1
            && completion["exit_code"].as_i64() == Some(0)
            && completion
                .get("diagnostic_only")
                .is_none_or(|value| value == false)
            && completion["worker_quiescence"]["remaining_workers"].as_u64() == Some(0)
            && completion["worker_quiescence"]["quiet_scans"]
                .as_u64()
                .is_some_and(|count| count >= 2),
        R::MetadataInvalid
    );
    let launch = completion["launch_sha256"]
        .as_str()
        .context(R::MetadataInvalid)?;
    ensure!(
        launch.len() == 64
            && launch
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        R::MetadataInvalid
    );
    let generation = completion["generation"]
        .as_str()
        .context(R::MetadataInvalid)?;
    ensure!(
        generation.starts_with("generation-")
            && generation.len() > 11
            && generation
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
        R::MetadataInvalid
    );
    let aliases = root.join(&declared.provider).join("aliases");
    private_directory(&aliases)?;
    let alias = super::private(&aliases.join(sha(declared.native_session_id.as_bytes())))?;
    let target: String = serde_json::from_slice(&alias)?;
    ensure!(
        target.starts_with("session-")
            && target
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            && session.file_name().and_then(|name| name.to_str()) == Some(&target),
        R::MetadataInvalid
    );
    let manifest = super::owner_inventory::export_manifest(&session.join(generation))?;
    let actual = RuntimeInventoryRow {
        provider: declared.provider.clone(),
        native_session_id: declared.native_session_id.clone(),
        runtime_relative_path: declared.runtime_relative_path.clone(),
        creation_owner,
        creation_audit_path,
        effective_owner: effective.owner,
        effective_audit_path: effective.audit_path,
        creation_receipt_sha256: sha(&owned),
        completion_sha256: sha(&complete),
        alias_sha256: sha(&alias),
        export_manifest_sha256: value_hash(&manifest)?,
    };
    ensure!(actual == *declared, R::MetadataInvalid);
    Ok(())
}
pub(super) fn handoff_at(
    root: &Path,
    max_state_bytes: u64,
    request: &TransferRequest,
    sources: &[Arc<AuditLogger>],
    destination: &Arc<AuditLogger>,
    current: &dyn Fn() -> Result<()>,
) -> Result<TransferReceipt> {
    current()?;
    request.validate()?;
    let lock = super::StateLock::acquire(root)?;
    let mut ledger = Ledger::open(root, &lock)?;
    match ledger.state_for(request)? {
        Some(State::Completed(receipt)) => {
            receipt.verify(request)?;
            return Ok(receipt);
        }
        Some(State::Aborted) => anyhow::bail!(R::AuthorityConflict),
        _ => {}
    }
    let plan = audit_plan(request, sources, destination).context(R::AuditCorrupt)?;
    let paths = inventory_paths(root, &ledger, request)?;
    let _sessions = paths
        .iter()
        .map(|path| session_lock(path))
        .collect::<Result<Vec<_>>>()?;
    for row in &request.inventory {
        exact_inventory_row(root, &ledger, row)?;
    }
    current()?;
    // Read-only destination validation does not append, so overlap/busy/invalid
    // requests leave both source and destination audit bytes unchanged.
    destination
        .handoff_destination_ready(&request.destination.owner)
        .context(R::AuditCorrupt)?;
    ensure!(
        matches!(
            ledger.prepare(request, &plan.endpoints, max_state_bytes)?,
            State::Prepared
        ),
        R::AuthorityConflict
    );
    let digest = request.request_digest()?;
    let mut seals = Vec::new();
    for source in plan.sources {
        current()?;
        let proof = source
            .logger
            .handoff_evidence(&digest, &request.destination.owner, current)
            .context(R::AuditCorrupt)?;
        let seal_sha256 = value_hash(&proof)?;
        for audit_path in source.references {
            seals.push(SealReference {
                audit_path,
                seal_sha256: seal_sha256.clone(),
            });
        }
    }
    seals.sort_by(|a, b| a.audit_path.as_bytes().cmp(b.audit_path.as_bytes()));
    current()?;
    ledger.commit(request, seals)
}
#[cfg(test)]
mod tests {
    use super::super::ownership::{
        TransferFenceError, TransferObservation, TransferOutcome, TransferReason,
    };
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;
    fn observation() -> TransferObservation {
        TransferObservation {
            revision: 1,
            sha256: "a".repeat(64),
            certain: true,
        }
    }
    fn policy(f: &Fixture) -> PathBuf {
        let path = f._temporary.path().join("policy.json");
        let value = json!({"version":1,"runtime_config":"/unused-runtime","guest_runtime":"/unused-guest","state_root":f.root,"memory_mb":512,"max_state_bytes":u64::MAX,"max_sessions":8,"providers":{}});
        std::fs::write(&path, &serde_json::to_vec(&value).unwrap()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        path
    }
    fn outcome(
        f: &Fixture,
        observation: &TransferObservation,
        current: &dyn Fn() -> std::result::Result<(), TransferFenceError>,
    ) -> TransferOutcome {
        super::super::owner_outcome::transfer_with_policy(
            Some(&policy(f)),
            &f.request,
            observation,
            &f.sources,
            &f.destination,
            current,
        )
    }
    #[test]
    fn native_handoff_outcome_public_entrypoint_uses_selected_policy_and_replays_private_receipt() {
        const CHILD: &str = "JARVIS_TEST_TYPED_TRANSFER_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "native_vm::owner_handoff::tests::native_handoff_outcome_public_entrypoint_uses_selected_policy_and_replays_private_receipt", "--test-threads=1"])
                .env(CHILD,"1").env_remove("AUGMENTAGENT_NATIVE_VM_POLICY").output().unwrap();
            assert!(
                output.status.success(),
                "public entrypoint child failed: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let f = fixture(true);
        std::env::set_var("AUGMENTAGENT_NATIVE_VM_POLICY", policy(&f));
        let binding =
            super::super::ownership::capture_transfer_binding(&f.request, &observation(), &|| {
                Ok(())
            })
            .unwrap();
        let result = super::super::ownership::transfer(
            &binding,
            &f.request,
            &observation(),
            &f.sources,
            &f.destination,
            &|| Ok(()),
        );
        let TransferOutcome::Completed(receipt) = &result else {
            panic!("valid selected-policy transfer did not complete: {result:?}")
        };
        receipt.verify(&f.request).unwrap();
        let public = result.public_status();
        assert_eq!(public.as_object().unwrap().len(), 3);
        assert!(!public.to_string().contains(f.root.to_str().unwrap()));
        assert!(!public.to_string().contains("native-codex"));
        for source in &f.sources {
            std::fs::remove_file(source.path()).unwrap();
        }
        assert_eq!(
            super::super::ownership::transfer(
                &binding,
                &f.request,
                &observation(),
                &f.sources,
                &f.destination,
                &|| Ok(())
            ),
            result
        );
    }
    #[test]
    fn native_handoff_outcome_preflight_distinguishes_stale_unavailable_and_uncertain_without_mutation(
    ) {
        for case in ["stale", "unavailable", "uncertain", "invalid"] {
            let mut f = fixture(false);
            let before = audit_bytes(&f);
            let native_before = std::fs::read(f.root.join("runtime-retirement.sqlite")).unwrap();
            let mut observed = observation();
            if case == "uncertain" {
                observed.certain = false;
            }
            if case == "invalid" {
                f.request.version = 0;
            }
            let result = outcome(&f, &observed, &|| match case {
                "stale" => Err(TransferFenceError::NotCurrent),
                "unavailable" => Err(TransferFenceError::Unavailable),
                _ => Ok(()),
            });
            match (case, result) {
                (
                    "stale",
                    TransferOutcome::Held {
                        reason: TransferReason::AuthorityConflict,
                        ..
                    },
                )
                | (
                    "unavailable",
                    TransferOutcome::Deferred {
                        reason: TransferReason::PersistRetry,
                        ..
                    },
                )
                | (
                    "uncertain",
                    TransferOutcome::Held {
                        reason: TransferReason::ConversationUncertain,
                        ..
                    },
                )
                | (
                    "invalid",
                    TransferOutcome::Held {
                        reason: TransferReason::InvalidRequest,
                        ..
                    },
                ) => (),
                (_, other) => panic!("wrong {case} outcome: {other:?}"),
            }
            assert_eq!(audit_bytes(&f), before);
            assert_eq!(
                std::fs::read(f.root.join("runtime-retirement.sqlite")).unwrap(),
                native_before
            );
        }
    }
    #[test]
    fn native_handoff_outcome_busy_fingerprint_tracks_observation_policy_and_request() {
        let mut f = fixture(false);
        let before = audit_bytes(&f);
        let _held =
            session_lock(&f.root.join(&f.request.inventory[0].runtime_relative_path)).unwrap();
        let policy_path = policy(&f);
        let run = |f: &Fixture, observed: &TransferObservation| {
            super::super::owner_outcome::transfer_with_policy(
                Some(&policy_path),
                &f.request,
                observed,
                &f.sources,
                &f.destination,
                &|| Ok(()),
            )
        };
        let observed = observation();
        let first = run(&f, &observed);
        assert!(
            matches!(&first, TransferOutcome::Deferred {reason:TransferReason::LockBusy, fingerprint} if fingerprint.len()==64)
        );
        assert_eq!(run(&f, &observed), first);
        let mut newer = observed.clone();
        newer.revision += 1;
        assert_ne!(run(&f, &newer), first);
        newer = observed.clone();
        newer.sha256 = "b".repeat(64);
        assert_ne!(run(&f, &newer), first);
        let mut selected: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&policy_path).unwrap()).unwrap();
        selected["max_sessions"] = json!(9);
        std::fs::write(&policy_path, &serde_json::to_vec(&selected).unwrap()).unwrap();
        assert_ne!(run(&f, &observed), first);
        policy(&f);
        f.request.inventory[0].completion_sha256 = "c".repeat(64);
        assert_ne!(run(&f, &observed), first);
        f.request.inventory[0].completion_sha256 = "d".repeat(64);
        f.request.inventory[0].alias_sha256 = "e".repeat(64);
        assert_ne!(run(&f, &observed), first);
        assert_eq!(audit_bytes(&f), before);
        assert!(!first
            .public_status()
            .to_string()
            .contains(f.root.to_str().unwrap()));
    }
    #[test]
    fn native_handoff_outcome_corrupt_audit_holds_and_unreadable_pending_state_is_unknown() {
        let f = fixture(false);
        std::fs::write(f.sources[0].path(), b"broken private audit fixture").unwrap();
        let result = outcome(&f, &observation(), &|| Ok(()));
        assert!(
            matches!(
                &result,
                TransferOutcome::Held {
                    reason: TransferReason::AuditCorrupt,
                    ..
                }
            ),
            "wrong corrupt-audit outcome: {result:?}"
        );
        let f = fixture(false);
        let corrupted = std::cell::Cell::new(false);
        let current = || {
            let db = f.root.join("runtime-retirement.sqlite");
            if db.exists() && !corrupted.get() {
                let connection = augmentagent_store::rusqlite::Connection::open(&db).unwrap();
                let prepared: bool = connection
                    .query_row("SELECT EXISTS(SELECT 1 FROM native_transfer)", [], |row| {
                        row.get(0)
                    })
                    .unwrap();
                drop(connection);
                if prepared {
                    std::fs::write(&db, b"broken private ledger fixture").unwrap();
                    corrupted.set(true);
                    return Err(TransferFenceError::Unavailable);
                }
            }
            Ok(())
        };
        assert_eq!(
            outcome(&f, &observation(), &current),
            TransferOutcome::Unknown {
                reason: TransferReason::PendingUnknown,
                transfer_id: f.request.transfer_id.clone()
            }
        );
        assert!(corrupted.get());
    }
    #[test]
    fn native_handoff_outcome_live_writer_defers_then_replays_but_interrupted_writer_holds() {
        let f = fixture(false);
        let writer = f.sources[0].begin_write().unwrap();
        assert!(matches!(
            outcome(&f, &observation(), &|| Ok(())),
            TransferOutcome::Deferred {
                reason: TransferReason::LockBusy,
                ..
            }
        ));
        writer.complete();
        assert!(matches!(
            outcome(&f, &observation(), &|| Ok(())),
            TransferOutcome::Completed(_)
        ));
        let f = fixture(false);
        drop(f.sources[0].begin_write().unwrap());
        assert!(matches!(
            outcome(&f, &observation(), &|| Ok(())),
            TransferOutcome::Held {
                reason: TransferReason::AuditCorrupt,
                ..
            }
        ));
    }
    #[test]
    fn native_handoff_outcome_committed_receipt_wins_response_error_and_changed_uuid_inputs_conflict(
    ) {
        let mut f = fixture(false);
        let receipt = handoff_at(
            &f.root,
            u64::MAX,
            &f.request,
            &f.sources,
            &f.destination,
            &|| Ok(()),
        )
        .unwrap();
        let observed = observation();
        let mut context =
            super::super::owner_outcome::FingerprintContext::new(&f.request, &observed);
        let error = anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "private fixture response lost",
        ));
        assert_eq!(
            super::super::owner_outcome::reconcile_failure(
                &f.root,
                &f.request,
                &mut context,
                &error
            ),
            TransferOutcome::Completed(receipt.clone())
        );
        let original = f.request.clone();
        f.request.inventory[0].completion_sha256 = "1".repeat(64);
        let result = outcome(&f, &observed, &|| Ok(()));
        assert!(
            matches!(
                &result,
                TransferOutcome::Held {
                    reason: TransferReason::AuthorityConflict,
                    ..
                }
            ),
            "valid stored receipt mislabeled after UUID input mismatch: {result:?}"
        );
        f.request = original;
        assert_eq!(
            outcome(&f, &observed, &|| Ok(())),
            TransferOutcome::Completed(receipt)
        );
    }
    #[test]
    fn native_handoff_outcome_aborted_request_holds_without_reviving_runtime_authority() {
        let f = fixture(false);
        let current = || {
            let db = f.root.join("runtime-retirement.sqlite");
            if db.exists() {
                let connection = augmentagent_store::rusqlite::Connection::open(db).unwrap();
                let prepared: bool = connection
                    .query_row("SELECT EXISTS(SELECT 1 FROM native_transfer)", [], |row| {
                        row.get(0)
                    })
                    .unwrap();
                if prepared {
                    return Err(TransferFenceError::NotCurrent);
                }
            }
            Ok(())
        };
        assert!(matches!(
            outcome(&f, &observation(), &current),
            TransferOutcome::Held {
                reason: TransferReason::AuthorityConflict,
                ..
            }
        ));
        {
            let lock = super::super::StateLock::acquire(&f.root).unwrap();
            Ledger::open(&f.root, &lock)
                .unwrap()
                .abort_and_retire(
                    &f.request.source_owners[0].audit_path,
                    &f.request.source_owners[0].owner,
                )
                .unwrap();
        }
        let before = audit_bytes(&f);
        assert!(matches!(
            outcome(&f, &observation(), &|| Ok(())),
            TransferOutcome::Held {
                reason: TransferReason::AuthorityConflict,
                ..
            }
        ));
        assert_eq!(audit_bytes(&f), before);
        let lock = super::super::StateLock::acquire(&f.root).unwrap();
        let ledger = Ledger::open(&f.root, &lock).unwrap();
        assert_eq!(
            ledger.state(&f.request.transfer_id).unwrap(),
            Some(State::Aborted)
        );
        assert_eq!(ledger.reserved_bytes().unwrap(), 0);
    }
    fn empty_binding_fixture() -> Fixture {
        let mut f = fixture(false);
        for provider in ["codex", "claude"] {
            std::fs::remove_dir_all(f.root.join(provider)).unwrap();
        }
        f.request.inventory.clear();
        f
    }
    fn binding_for(f: &Fixture, path: &Path) -> super::super::ownership::TransferBinding {
        super::super::owner_outcome::capture_with_policy(
            Some(path),
            &f.request,
            &observation(),
            &|| Ok(()),
        )
        .unwrap()
    }
    fn run_bound(
        f: &Fixture,
        path: &Path,
        binding: &super::super::ownership::TransferBinding,
    ) -> TransferOutcome {
        super::super::owner_outcome::transfer_bound_with_policy(
            Some(path),
            binding,
            &f.request,
            &observation(),
            &f.sources,
            &f.destination,
            &|| Ok(()),
        )
    }
    fn binding_selection_change_refuses(root_change: bool) {
        let f = empty_binding_fixture();
        let path = policy(&f);
        let binding = binding_for(&f, &path);
        let writer = f.sources[0].begin_write().unwrap();
        assert!(matches!(
            run_bound(&f, &path, &binding),
            TransferOutcome::Deferred {
                reason: TransferReason::LockBusy,
                ..
            }
        ));
        writer.complete();
        let other = tempfile::tempdir().unwrap();
        directory(other.path());
        super::super::initialize_native_test_store(other.path());
        let shadow_before = std::fs::read(other.path().join("runtime-retirement.sqlite")).unwrap();
        let original_before = std::fs::read(f.root.join("runtime-retirement.sqlite")).unwrap();
        let audit_before = audit_bytes(&f);
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        if root_change {
            value["state_root"] = json!(other.path());
        } else {
            value["max_sessions"] = json!(9);
        }
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        let result = run_bound(&f, &path, &binding);
        assert!(
            matches!(
                &result,
                TransferOutcome::Held {
                    reason: TransferReason::AuthorityConflict,
                    ..
                }
            ),
            "pending request changed policy/store: {result:?}"
        );
        assert_eq!(
            std::fs::read(other.path().join("runtime-retirement.sqlite")).unwrap(),
            shadow_before
        );
        assert_eq!(
            std::fs::read(f.root.join("runtime-retirement.sqlite")).unwrap(),
            original_before
        );
        assert_eq!(audit_bytes(&f), audit_before);
    }
    #[test]
    fn native_handoff_binding_policy_change_cannot_resume_original_request() {
        binding_selection_change_refuses(false);
    }
    #[test]
    fn native_handoff_binding_root_change_never_interprets_absence_in_replacement_store() {
        binding_selection_change_refuses(true);
    }
    #[test]
    fn native_handoff_binding_historical_lookup_uses_original_store_without_selected_policy_or_certainty(
    ) {
        let f = empty_binding_fixture();
        let path = policy(&f);
        let binding = binding_for(&f, &path);
        let TransferOutcome::Completed(receipt) = run_bound(&f, &path, &binding) else {
            panic!("fixture transfer did not complete")
        };
        let persisted = serde_json::to_vec(&binding).unwrap();
        let restored = serde_json::from_slice(&persisted).unwrap();
        std::fs::remove_file(&path).unwrap();
        for source in &f.sources {
            std::fs::remove_file(source.path()).unwrap();
        }
        let before = std::fs::read(f.root.join("runtime-retirement.sqlite")).unwrap();
        let mut uncertain = observation();
        uncertain.certain = false;
        assert_eq!(
            super::super::ownership::transfer_receipt(
                &restored,
                &f.request,
                &uncertain,
                &|| Ok(())
            ),
            Ok(Some(receipt))
        );
        assert_eq!(
            std::fs::read(f.root.join("runtime-retirement.sqlite")).unwrap(),
            before
        );
    }
    #[test]
    fn native_handoff_binding_prepared_lookup_is_read_only_and_missing_original_store_stays_unknown(
    ) {
        let f = empty_binding_fixture();
        let path = policy(&f);
        let binding = binding_for(&f, &path);
        let writer = f.sources[0].begin_write().unwrap();
        assert!(matches!(
            run_bound(&f, &path, &binding),
            TransferOutcome::Deferred {
                reason: TransferReason::LockBusy,
                ..
            }
        ));
        writer.complete();
        let before = std::fs::read(f.root.join("runtime-retirement.sqlite")).unwrap();
        assert_eq!(
            super::super::ownership::transfer_receipt(
                &binding,
                &f.request,
                &observation(),
                &|| Ok(())
            ),
            Ok(None)
        );
        assert_eq!(
            std::fs::read(f.root.join("runtime-retirement.sqlite")).unwrap(),
            before
        );
        std::fs::remove_file(f.root.join("runtime-retirement.sqlite")).unwrap();
        let expected = TransferOutcome::Unknown {
            reason: TransferReason::PendingUnknown,
            transfer_id: f.request.transfer_id.clone(),
        };
        assert_eq!(
            super::super::ownership::transfer_receipt(
                &binding,
                &f.request,
                &observation(),
                &|| Ok(())
            ),
            Err(expected.clone())
        );
        assert_eq!(run_bound(&f, &path, &binding), expected);
        assert!(!f.root.join("runtime-retirement.sqlite").exists());
    }
    #[test]
    fn native_handoff_binding_missing_authority_tables_are_never_recreated() {
        let f = empty_binding_fixture();
        let path = policy(&f);
        let binding = binding_for(&f, &path);
        assert!(matches!(
            run_bound(&f, &path, &binding),
            TransferOutcome::Completed(_)
        ));
        {
            let _lock = super::super::StateLock::acquire(&f.root).unwrap();
            let db = augmentagent_store::rusqlite::Connection::open(
                f.root.join("runtime-retirement.sqlite"),
            )
            .unwrap();
            db.execute_batch("DROP TABLE native_transfer_link;DROP TABLE native_transfer_close;DROP TABLE native_transfer_claim;DROP TABLE native_transfer_source;DROP TABLE native_transfer;DROP TABLE native_transfer_meta;").unwrap();
        }
        let before = std::fs::read(f.root.join("runtime-retirement.sqlite")).unwrap();
        let expected = TransferOutcome::Unknown {
            reason: TransferReason::PendingUnknown,
            transfer_id: f.request.transfer_id.clone(),
        };
        assert_eq!(run_bound(&f, &path, &binding), expected);
        assert_eq!(
            std::fs::read(f.root.join("runtime-retirement.sqlite")).unwrap(),
            before
        );
    }
    #[test]
    fn native_handoff_binding_policy_change_during_sealing_preserves_exact_prepared_request() {
        let f = fixture(true);
        let path = policy(&f);
        let binding = binding_for(&f, &path);
        let original = std::fs::read(&path).unwrap();
        let changed = std::cell::Cell::new(false);
        let current = || {
            let value: serde_json::Value = serde_json::from_slice(
                &std::fs::read(f.sources[0].protected_anchor_path().unwrap()).unwrap(),
            )
            .unwrap();
            if !value["sealed"].is_null() && !changed.replace(true) {
                let mut bytes = original.clone();
                bytes.push(b' ');
                std::fs::write(&path, bytes).unwrap();
            }
            Ok(())
        };
        let result = super::super::owner_outcome::transfer_bound_with_policy(
            Some(&path),
            &binding,
            &f.request,
            &observation(),
            &f.sources,
            &f.destination,
            &current,
        );
        assert!(
            matches!(
                &result,
                TransferOutcome::Held {
                    reason: TransferReason::AuthorityConflict,
                    ..
                }
            ),
            "changed policy did not fence seal/commit: {result:?}"
        );
        assert!(changed.get());
        assert_eq!(
            super::super::ownership::transfer_receipt(
                &binding,
                &f.request,
                &observation(),
                &|| Ok(())
            ),
            Ok(None)
        );
        std::fs::write(&path, &original).unwrap();
        assert!(matches!(
            run_bound(&f, &path, &binding),
            TransferOutcome::Completed(_)
        ));
    }
    #[test]
    fn native_handoff_binding_capture_rechecks_current_authority_before_returning() {
        let f = empty_binding_fixture();
        let path = policy(&f);
        let calls = std::cell::Cell::new(0);
        let result = super::super::owner_outcome::capture_with_policy(
            Some(&path),
            &f.request,
            &observation(),
            &|| {
                calls.set(calls.get() + 1);
                if calls.get() > 1 {
                    Err(TransferFenceError::NotCurrent)
                } else {
                    Ok(())
                }
            },
        );
        assert!(
            matches!(
                &result,
                Err(TransferOutcome::Held {
                    reason: TransferReason::AuthorityConflict,
                    ..
                })
            ),
            "capture returned after its lease became stale: {result:?}"
        );
    }
    struct Fixture {
        _temporary: tempfile::TempDir,
        root: PathBuf,
        request: TransferRequest,
        sources: Vec<Arc<AuditLogger>>,
        destination: Arc<AuditLogger>,
    }
    fn directory(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    fn logger(root: &Path, name: &str, generation: i64) -> Arc<AuditLogger> {
        let log = root.join("logs").join(format!("{name}.jsonl"));
        let anchor = root.join("anchors").join(format!("{name}.json"));
        let logger = Arc::new(AuditLogger::protected(log, anchor, &[]).unwrap());
        logger
            .set_native_runtime_owner(super::super::RuntimeOwner {
                workflow_id: "workflow".into(),
                lease_generation: generation,
            })
            .unwrap();
        logger.record_controller(json!({"actor":"controller","kind":"workflow_claimed","workflow":"workflow","generation":generation})).unwrap();
        logger.require_run_resources().unwrap();
        logger
    }
    fn fixture(two_sources: bool) -> Fixture {
        let temporary = tempfile::tempdir().unwrap();
        directory(temporary.path());
        let base = temporary.path().canonicalize().unwrap();
        let root = base.join("native");
        directory(&root);
        super::super::initialize_native_test_store(&root);
        let audit = base.join("audit");
        directory(&audit.join("logs"));
        directory(&audit.join("anchors"));
        let mut sources = vec![logger(&audit, "source-a", 1)];
        if two_sources {
            sources.push(logger(&audit, "source-b", 2));
        }
        let destination = logger(&audit, "destination", 3);
        let source_owners = sources
            .iter()
            .map(|logger| Authority {
                owner: logger.native_runtime_owner().unwrap().unwrap(),
                audit_path: logger.path().to_str().unwrap().into(),
            })
            .collect::<Vec<_>>();
        let mut inventory = Vec::new();
        for (index, provider) in ["codex", "claude"].into_iter().enumerate() {
            let owner = &source_owners[if two_sources { index } else { 0 }];
            let native_session_id = format!("native-{provider}");
            let runtime_relative_path = format!("{provider}/session-{provider}");
            let session = root.join(&runtime_relative_path);
            directory(&root.join(provider));
            directory(&session);
            directory(&root.join(provider).join("aliases"));
            let owned = serde_json::to_vec(
                &json!({"version":1,"owner":owner.owner,"audit_path":owner.audit_path}),
            )
            .unwrap();
            super::super::write(&session.join("owned.json"), &owned).unwrap();
            let complete=serde_json::to_vec(&json!({"version":1,"exit_code":0,"generation":"generation-one","launch_sha256":"f".repeat(64),"worker_quiescence":{"remaining_workers":0,"quiet_scans":2}})).unwrap();
            super::super::write(&session.join("complete.json"), &complete).unwrap();
            super::super::write(&session.join("lock"), b"").unwrap();
            directory(&session.join("generation-one"));
            super::super::write(
                &session.join("generation-one/context"),
                format!("saved {provider}").as_bytes(),
            )
            .unwrap();
            let alias = serde_json::to_vec(&format!("session-{provider}")).unwrap();
            super::super::write(
                &root
                    .join(provider)
                    .join("aliases")
                    .join(sha(native_session_id.as_bytes())),
                &alias,
            )
            .unwrap();
            inventory.push(RuntimeInventoryRow {
                provider: provider.into(),
                native_session_id,
                runtime_relative_path,
                creation_owner: owner.owner.clone(),
                creation_audit_path: owner.audit_path.clone(),
                effective_owner: owner.owner.clone(),
                effective_audit_path: owner.audit_path.clone(),
                creation_receipt_sha256: sha(&owned),
                completion_sha256: sha(&complete),
                alias_sha256: sha(&alias),
                export_manifest_sha256: value_hash(
                    &super::super::owner_inventory::export_manifest(
                        &session.join("generation-one"),
                    )
                    .unwrap(),
                )
                .unwrap(),
            });
        }
        inventory.sort_by(|a, b| {
            (&a.provider, &a.runtime_relative_path).cmp(&(&b.provider, &b.runtime_relative_path))
        });
        let request = TransferRequest {
            version: 1,
            transfer_id: uuid::Uuid::new_v4().to_string(),
            workflow_id: "workflow".into(),
            source_owners,
            destination: Authority {
                owner: destination.native_runtime_owner().unwrap().unwrap(),
                audit_path: destination.path().to_str().unwrap().into(),
            },
            inventory,
        };
        Fixture {
            _temporary: temporary,
            root,
            request,
            sources,
            destination,
        }
    }
    fn audit_bytes(fixture: &Fixture) -> Vec<(Vec<u8>, Vec<u8>)> {
        fixture
            .sources
            .iter()
            .chain(std::iter::once(&fixture.destination))
            .map(|logger| {
                (
                    std::fs::read(logger.path()).unwrap(),
                    std::fs::read(logger.protected_anchor_path().unwrap()).unwrap(),
                )
            })
            .collect()
    }
    #[test]
    fn native_handoff_seals_real_audits_and_preserves_native_runtime_bytes() {
        let f = fixture(true);
        let before = f
            .request
            .inventory
            .iter()
            .map(|row| {
                std::fs::read(f.root.join(&row.runtime_relative_path).join("owned.json")).unwrap()
            })
            .collect::<Vec<_>>();
        let receipt = handoff_at(
            &f.root,
            u64::MAX,
            &f.request,
            &f.sources,
            &f.destination,
            &|| Ok(()),
        )
        .unwrap();
        receipt.verify(&f.request).unwrap();
        for source in &f.sources {
            assert!(
                source
                    .record_controller(json!({"actor":"controller","kind":"late"}))
                    .is_err(),
                "handoff did not close its source audit"
            );
        }
        assert_eq!(
            handoff_at(&f.root, 0, &f.request, &f.sources, &f.destination, &|| Ok(
                ()
            ))
            .unwrap(),
            receipt
        );
        for (row, before) in f.request.inventory.iter().zip(before) {
            let path = f.root.join(&row.runtime_relative_path);
            assert_eq!(std::fs::read(path.join("owned.json")).unwrap(), before);
            assert!(!path.join("retired-owner.json").exists());
            assert!(!path.join("retired.json").exists());
        }
    }
    #[test]
    fn native_handoff_refuses_incomplete_or_changed_inventory_before_audit_sealing() {
        for case in [
            "omitted-runtime",
            "changed-alias",
            "changed-export",
            "failed-completion",
            "missing-completion",
        ] {
            let mut f = fixture(false);
            let row = &f.request.inventory[0];
            let session = f.root.join(&row.runtime_relative_path);
            match case {
                "omitted-runtime" => {
                    f.request.inventory.pop();
                }
                "changed-alias" => {
                    std::fs::write(
                        f.root
                            .join(&row.provider)
                            .join("aliases")
                            .join(sha(row.native_session_id.as_bytes())),
                        b"\"session-other\"",
                    )
                    .unwrap();
                }
                "changed-export" => {
                    std::fs::write(session.join("generation-one/context"), b"changed").unwrap();
                }
                "failed-completion" => {
                    std::fs::write(
                        session.join("complete.json"),
                        b"{\"version\":1,\"exit_code\":1}",
                    )
                    .unwrap();
                }
                "missing-completion" => {
                    std::fs::remove_file(session.join("complete.json")).unwrap();
                }
                _ => unreachable!(),
            }
            let before = audit_bytes(&f);
            assert!(
                handoff_at(
                    &f.root,
                    u64::MAX,
                    &f.request,
                    &f.sources,
                    &f.destination,
                    &|| Ok(())
                )
                .is_err(),
                "accepted {case}"
            );
            assert_eq!(audit_bytes(&f), before);
            let lock = super::super::StateLock::acquire(&f.root).unwrap();
            assert!(Ledger::open(&f.root, &lock)
                .unwrap()
                .state(&f.request.transfer_id)
                .unwrap()
                .is_none());
        }
    }
    #[test]
    fn native_handoff_busy_runtime_changes_no_audit_or_owner() {
        let f = fixture(false);
        let row = &f.request.inventory[0];
        let held = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(f.root.join(&row.runtime_relative_path).join("lock"))
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let before = audit_bytes(&f);
        let result = handoff_at(
            &f.root,
            u64::MAX,
            &f.request,
            &f.sources,
            &f.destination,
            &|| Ok(()),
        );
        assert_eq!(unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_UN) }, 0);
        assert!(
            result.is_err(),
            "handoff ignored a live native session lock"
        );
        assert_eq!(audit_bytes(&f), before);
    }
    fn sealed_count(f: &Fixture) -> usize {
        f.sources
            .iter()
            .filter(|logger| {
                serde_json::from_slice::<serde_json::Value>(
                    &std::fs::read(logger.protected_anchor_path().unwrap()).unwrap(),
                )
                .unwrap()["sealed"]
                    .is_object()
            })
            .count()
    }
    #[test]
    fn native_handoff_process_crash_after_prepare_each_seal_and_commit_replays_safely() {
        const TEST:&str="native_vm::owner_handoff::tests::native_handoff_process_crash_after_prepare_each_seal_and_commit_replays_safely";
        if let Some(root) = std::env::var_os("AUTOSHIP_HANDOFF_CRASH_ROOT") {
            let root = PathBuf::from(root);
            let base = root.parent().unwrap();
            let request =
                TransferRequest::parse(&super::super::private(&base.join("request.json")).unwrap())
                    .unwrap();
            let mut sources = Vec::new();
            let mut anchors = Vec::new();
            for owner in &request.source_owners {
                let path = PathBuf::from(&owner.audit_path);
                let anchor = base
                    .join("audit/anchors")
                    .join(path.file_stem().unwrap())
                    .with_extension("json");
                let logger = Arc::new(AuditLogger::protected(path, anchor.clone(), &[]).unwrap());
                logger
                    .set_native_runtime_owner(owner.owner.clone())
                    .unwrap();
                sources.push(logger);
                anchors.push(anchor);
            }
            let destination = Arc::new(
                AuditLogger::protected(
                    PathBuf::from(&request.destination.audit_path),
                    base.join("audit/anchors/destination.json"),
                    &[],
                )
                .unwrap(),
            );
            destination
                .set_native_runtime_owner(request.destination.owner.clone())
                .unwrap();
            let phase = std::env::var("AUTOSHIP_HANDOFF_CRASH_PHASE").unwrap();
            let current = || -> Result<()> {
                let sealed = anchors
                    .iter()
                    .filter(|path| {
                        serde_json::from_slice::<serde_json::Value>(&std::fs::read(path).unwrap())
                            .unwrap()["sealed"]
                            .is_object()
                    })
                    .count();
                let prepared = if root.join("runtime-retirement.sqlite").is_file() {
                    let db = augmentagent_store::rusqlite::Connection::open_with_flags(
                        root.join("runtime-retirement.sqlite"),
                        augmentagent_store::rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                    )
                    .unwrap();
                    db.query_row("SELECT EXISTS(SELECT 1 FROM native_transfer WHERE id=?1 AND state='prepared')",[&request.transfer_id],|row|row.get::<_,bool>(0)).unwrap_or(false)
                } else {
                    false
                };
                if (phase == "prepared" && prepared && sealed == 0)
                    || (phase == "one-seal" && sealed == 1)
                    || (phase == "all-seals" && sealed == 2)
                {
                    unsafe { libc::_exit(0) }
                }
                Ok(())
            };
            handoff_at(&root, u64::MAX, &request, &sources, &destination, &current).unwrap();
            assert_eq!(phase, "committed", "requested crash point was not reached");
            unsafe { libc::_exit(0) }
        }
        for (phase, expected_seals) in [
            ("prepared", 0),
            ("one-seal", 1),
            ("all-seals", 2),
            ("committed", 2),
        ] {
            let f = fixture(true);
            let before = f
                .request
                .inventory
                .iter()
                .map(|row| {
                    std::fs::read(f.root.join(&row.runtime_relative_path).join("owned.json"))
                        .unwrap()
                })
                .collect::<Vec<_>>();
            super::super::write(
                &f.root.parent().unwrap().join("request.json"),
                &canonical_bytes(&serde_json::to_value(&f.request).unwrap()).unwrap(),
            )
            .unwrap();
            let child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture"])
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("AUTOSHIP_HANDOFF_CRASH_ROOT", &f.root)
                .env("AUTOSHIP_HANDOFF_CRASH_PHASE", phase)
                .output()
                .unwrap();
            assert!(
                child.status.success(),
                "crash fixture {phase} failed: {}",
                String::from_utf8_lossy(&child.stderr)
            );
            assert_eq!(sealed_count(&f), expected_seals);
            let preserved = f
                .sources
                .iter()
                .filter_map(|logger| {
                    let path = logger.protected_anchor_path().unwrap();
                    let bytes = std::fs::read(path).unwrap();
                    serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["sealed"]
                        .is_object()
                        .then(|| (path.to_owned(), bytes))
                })
                .collect::<Vec<_>>();
            if expected_seals > 0 {
                assert!(f.sources[0]
                    .record_controller(json!({"actor":"controller","kind":"rejected-old-writer"}))
                    .is_err());
            }
            let receipt = handoff_at(
                &f.root,
                u64::MAX,
                &f.request,
                &f.sources,
                &f.destination,
                &|| Ok(()),
            )
            .unwrap();
            receipt.verify(&f.request).unwrap();
            assert_eq!(sealed_count(&f), 2);
            for (path, bytes) in preserved {
                assert_eq!(
                    std::fs::read(path).unwrap(),
                    bytes,
                    "replay rewrote a committed seal"
                );
            }
            for (row, bytes) in f.request.inventory.iter().zip(before) {
                assert_eq!(
                    std::fs::read(f.root.join(&row.runtime_relative_path).join("owned.json"))
                        .unwrap(),
                    bytes
                );
            }
            // Historical completion survives normal, authorized runtime GC.
            {
                let lock = super::super::StateLock::acquire(&f.root).unwrap();
                super::super::retire_owned_at(
                    &f.root,
                    &lock,
                    Path::new(&f.request.destination.audit_path),
                    &f.request.destination.owner,
                )
                .unwrap();
                assert_eq!(super::super::prune_and_count(&f.root).unwrap(), 0);
            }
            assert_eq!(
                handoff_at(&f.root, 0, &f.request, &f.sources, &f.destination, &|| Ok(
                    ()
                ))
                .unwrap(),
                receipt
            );
        }
    }
    #[test]
    fn native_handoff_current_lease_loss_after_one_seal_retains_exact_pending_request() {
        let f = fixture(true);
        let current = || -> Result<()> {
            ensure!(sealed_count(&f) == 0, "synthetic workflow lease superseded");
            Ok(())
        };
        assert!(handoff_at(
            &f.root,
            u64::MAX,
            &f.request,
            &f.sources,
            &f.destination,
            &current
        )
        .is_err());
        assert_eq!(sealed_count(&f), 1);
        {
            let lock = super::super::StateLock::acquire(&f.root).unwrap();
            assert_eq!(
                Ledger::open(&f.root, &lock)
                    .unwrap()
                    .state(&f.request.transfer_id)
                    .unwrap(),
                Some(State::Prepared)
            );
        }
        handoff_at(
            &f.root,
            u64::MAX,
            &f.request,
            &f.sources,
            &f.destination,
            &|| Ok(()),
        )
        .unwrap();
        assert_eq!(sealed_count(&f), 2);
    }
    #[test]
    fn native_handoff_canonical_audit_alias_preserves_immutable_creation_provenance() {
        use std::os::unix::fs::symlink;
        let mut f = fixture(false);
        let alias = f.root.parent().unwrap().join("audit-alias");
        symlink(f.root.parent().unwrap().join("audit"), &alias).unwrap();
        let alias_log = alias
            .join("logs/source-a.jsonl")
            .to_str()
            .unwrap()
            .to_string();
        f.request.source_owners[0].audit_path = alias_log.clone();
        for row in &mut f.request.inventory {
            row.creation_audit_path = alias_log.clone();
            row.effective_audit_path = alias_log.clone();
            let path = f.root.join(&row.runtime_relative_path).join("owned.json");
            let bytes = serde_json::to_vec(
                &json!({"version":1,"owner":row.creation_owner,"audit_path":alias_log}),
            )
            .unwrap();
            std::fs::write(path, &bytes).unwrap();
            row.creation_receipt_sha256 = sha(&bytes);
        }
        let receipt = handoff_at(
            &f.root,
            u64::MAX,
            &f.request,
            &f.sources,
            &f.destination,
            &|| Ok(()),
        )
        .unwrap();
        receipt.verify(&f.request).unwrap();
        std::fs::remove_file(&alias).unwrap();
        assert_eq!(
            handoff_at(&f.root, 0, &f.request, &f.sources, &f.destination, &|| Ok(
                ()
            ))
            .unwrap(),
            receipt
        );
        for row in &f.request.inventory {
            let lock = super::super::StateLock::acquire(&f.root).unwrap();
            super::super::validate_resume_owner(
                &f.root,
                &lock,
                &f.root.join(&row.runtime_relative_path),
                Path::new(&f.request.destination.audit_path),
                &f.request.destination.owner,
            )
            .unwrap();
        }
    }
}
