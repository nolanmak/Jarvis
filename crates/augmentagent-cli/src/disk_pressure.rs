//! Pressure reclamation of rebuildable Cargo artifacts, never deployed outputs.
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use anyhow::{Context, Result};

use crate::{
    disk::{BuildProc, Root, RootReport},
    snapshot_refs::References,
    storage_leases::TargetLeases,
};

fn same_filesystem_tree(path: &Path, device: u64) -> bool {
    let Ok(meta) = path.symlink_metadata() else {
        return false;
    };
    if meta.dev() != device {
        return false;
    }
    if !meta.is_dir() {
        return true;
    } // Never follow nested symlinks.
    let Ok(entries) = std::fs::read_dir(path) else {
        return false;
    };
    entries
        .into_iter()
        .all(|entry| entry.is_ok_and(|entry| same_filesystem_tree(&entry.path(), device)))
}

/// The caller holds the admission lock, keeping reservations stable while we
/// measure and reclaim. Target leases remain nonblocking to avoid lock inversion.
pub fn reclaim(
    repo: &Path,
    required: u64,
    held: Option<&TargetLeases>,
    dry_run: bool,
    claims: &crate::storage_reservations::Claims,
    free_bytes: &dyn Fn(&Path) -> Option<u64>,
    admission_paths: Option<&[std::path::PathBuf]>,
) -> Result<Vec<RootReport>> {
    let procs = crate::disk::build_procs();
    let mut refs = crate::snapshot_refs::collect_rebuildable();
    // Worktree retention must also see compilers owned by other users and
    // refuse when build-process evidence is incomplete.
    if let Some(procs) = &procs {
        for process in procs {
            refs.paths.extend(process.cwd.iter().cloned());
            refs.config.push_str(&process.cmdline);
            refs.config.push('\n');
        }
    } else {
        refs.reliable = false;
    }
    // Worktree retirement is optional reclamation. Broken Git metadata or an
    // unreadable lane lock must keep those worktrees without preventing safe,
    // independently leased Cargo caches from restoring build headroom.
    let mut reports = match crate::autoship_worktree_cleanup::prune(repo, dry_run, &refs) {
        Ok(reports) => reports,
        Err(error) => vec![RootReport {
            label: "Autoship worktree".into(),
            path: repo.join(".self-improve-worktrees").display().to_string(),
            outcome: format!("kept: worktree cleanup unavailable: {error:#}"),
            removed: 0,
            freed_bytes: 0,
            full_drop: false,
        }],
    };
    let roots = crate::disk::discover_roots(
        repo,
        &crate::disk::extra_roots_from(
            &std::env::var("AUGMENTAGENT_PRUNE_TARGET_DIRS").unwrap_or_default(),
        ),
    );
    let headroom = |path: &Path| {
        Ok(free_bytes(path).context("cannot measure pressure filesystem")?
            .saturating_sub(claims.reserved_on(path)?))
    };
    let reclaimed = if let Some(paths) = admission_paths {
        reclaim_roots_for_paths(&roots, paths, required, held, dry_run,
            procs.as_deref(), &refs, &headroom)?
    } else {reclaim_roots(
        &roots,
        required,
        held,
        dry_run,
        procs.as_deref(),
        &refs,
        &headroom,
    )?};
    reports.extend(reclaimed);
    Ok(reports)
}

pub(crate) fn reclaim_roots_for_paths(
    roots: &[Root], _paths: &[std::path::PathBuf], required: u64,
    held: Option<&TargetLeases>, dry_run: bool, procs: Option<&[BuildProc]>,
    refs: &References, headroom: &dyn Fn(&Path) -> Result<u64>,
) -> Result<Vec<RootReport>> {
    reclaim_roots(roots, required, held, dry_run, procs, refs, headroom)
}

pub(crate) fn reclaim_roots(
    roots: &[Root],
    required: u64,
    held: Option<&TargetLeases>,
    dry_run: bool,
    procs: Option<&[BuildProc]>,
    refs: &References,
    headroom: &dyn Fn(&Path) -> Result<u64>,
) -> Result<Vec<RootReport>> {
    let mut reports = Vec::new();
    let mut projected_freed = std::collections::BTreeMap::<u64, u64>::new();
    for root in roots {
        // Discovery proves ownership; layout evidence prevents a replaced
        // checkout target from turning source or arbitrary data into a cache.
        if !crate::build_storage::known_cargo_layout(&root.path) {
            continue;
        }
        // A linked profile is not authority to pressure-delete its destination.
        // Independently registered targets can be reclaimed in their own turn.
        for profile in root.profiles.iter().filter(|p| p.starts_with(&root.path)) {
            if !profile.exists() {
                continue;
            }
            let device = std::fs::metadata(profile)?.dev();
            let simulated = if dry_run {
                projected_freed.get(&device).copied().unwrap_or(0)
            } else {
                0
            };
            if headroom(profile)?.saturating_add(simulated) >= required {
                continue;
            }
            let mut report = RootReport {
                label: "pressure cache".into(),
                path: profile.display().to_string(),
                outcome: String::new(),
                removed: 0,
                freed_bytes: 0,
                full_drop: false,
            };
            let lease = TargetLeases::try_profile(&root.path, profile, held)?;
            let stable = root.path.canonicalize().is_ok_and(|p| p == root.path)
                && profile.canonicalize().is_ok_and(|p| p == *profile);
            let resolve = |p: &Path| std::fs::canonicalize(p).ok();
            let compiler_busy = procs.is_none_or(|procs| {
                procs.iter().any(|p| {
                    crate::disk::root_in_use(&root.path, std::slice::from_ref(p), &resolve)
                        || crate::disk::root_in_use(profile, std::slice::from_ref(p), &resolve)
                        || p.target_arg
                            .as_ref()
                            .or(p.target_env.as_ref())
                            .is_some_and(|target| {
                                let target = if target.is_absolute() {
                                    target.clone()
                                } else if let Some(cwd) = &p.cwd {
                                    cwd.join(target)
                                } else {
                                    return true;
                                };
                                [target.clone(), target.join("debug"), target.join("release")]
                                    .iter()
                                    .any(|path| resolve(path).as_ref() == Some(profile))
                            })
                })
            });
            let lane_busy = root.gate
                && crate::disk::lane_locks()
                    .iter()
                    .any(|p| crate::disk::lock_is_held(p))
                && !held.is_some_and(|h| h.owns_directory(&root.path));
            let blocked = if lease.is_none() {
                Some("target lease is held")
            } else if !stable {
                Some("target/profile path changed")
            } else if procs.is_none() {
                Some("build-process evidence unavailable")
            } else if compiler_busy {
                Some("compiler is using this target")
            } else if lane_busy {
                Some("Autoship lane is using the gate cache")
            } else if !refs.reliable {
                Some("process/service reference evidence unavailable")
            } else {
                None
            };
            if let Some(reason) = blocked {
                report.outcome = format!("pressure skipped: {reason}");
                reports.push(report);
                continue;
            }
            let mut projected = headroom(profile)?.saturating_add(simulated);
            for name in ["incremental", "build", "deps", ".fingerprint"] {
                let before = if dry_run {
                    projected
                } else {
                    headroom(profile)?
                };
                if before >= required {
                    break;
                }
                let path = profile.join(name);
                let Ok(meta) = std::fs::symlink_metadata(&path) else {
                    continue;
                };
                if !meta.is_dir() || meta.file_type().is_symlink() || refs.protects(&path) {
                    continue;
                }
                if !same_filesystem_tree(&path, device) {
                    continue;
                }
                if dry_run {
                    let bytes = crate::disk::disk_bytes(&path);
                    report.freed_bytes = report.freed_bytes.saturating_add(bytes);
                    projected = projected.saturating_add(bytes);
                } else {
                    // remove_dir_all does not follow nested symlinks. Never
                    // remove the profile itself or its deployed executables.
                    if std::fs::remove_dir_all(&path).is_err() {
                        continue;
                    }
                    report.freed_bytes = report
                        .freed_bytes
                        .saturating_add(headroom(profile)?.saturating_sub(before));
                }
                report.removed += 1;
            }
            let after = if dry_run {
                projected
            } else {
                headroom(profile)?
            };
            report.outcome = format!(
                "pressure {} {} cache directories; remaining deficit {} bytes{}",
                if dry_run { "would remove" } else { "removed" },
                report.removed,
                required.saturating_sub(after),
                if dry_run { " (estimated)" } else { "" }
            );
            if dry_run {
                projected_freed.insert(device, simulated.saturating_add(report.freed_bytes));
            }
            reports.push(report);
        }
    }
    Ok(reports)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    struct Fixture {
        _dir: tempfile::TempDir,
        root: Root,
        deps: std::path::PathBuf,
        binary: std::path::PathBuf,
        refs: References,
    }

    impl Fixture {
        fn new() -> Self {
            Self::in_directory(tempfile::tempdir().unwrap())
        }
        fn in_directory(dir: tempfile::TempDir) -> Self {
            let target = dir.path().join("target");
            let deps = target.join("release/deps");
            std::fs::create_dir_all(&deps).unwrap();
            std::fs::write(
                target.join("CACHEDIR.TAG"),
                "Signature: 8a477f597d28d172789f06886806bc55\n",
            )
            .unwrap();
            std::fs::write(deps.join("warm.rlib"), vec![7; 65536]).unwrap();
            let binary = target.join("release/augmentagent");
            std::fs::write(&binary, b"live executable").unwrap();
            std::fs::write(target.join("release/unknown"), b"preserve").unwrap();
            let root = Root::at("checkout", &target);
            Self {
                _dir: dir,
                root,
                deps,
                binary,
                refs: References {
                    reliable: true,
                    ..Default::default()
                },
            }
        }

        fn run(
            &self,
            dry: bool,
            held: Option<&TargetLeases>,
            procs: Option<&[BuildProc]>,
            refs: &References,
        ) -> Vec<RootReport> {
            reclaim_roots(
                std::slice::from_ref(&self.root),
                65536,
                held,
                dry,
                procs,
                refs,
                &|_| Ok(if self.deps.exists() { 0 } else { 65536 }),
            )
            .unwrap()
        }
    }

    #[cfg(target_os="linux")]
    #[test]
    fn admission_pressure_preserves_an_unrelated_filesystem_even_when_both_are_low() {
        for dry_run in [true,false] {
            let requested=Fixture::new();
            let unrelated=Fixture::in_directory(tempfile::tempdir_in("/dev/shm").unwrap());
            assert_ne!(std::fs::metadata(&requested.root.path).unwrap().dev(),
                std::fs::metadata(&unrelated.root.path).unwrap().dev(),"fixture needs two real filesystems");
            let reports=reclaim_roots_for_paths(&[unrelated.root.clone(),requested.root.clone()],
                &[requested.root.path.join("future/output")],65536,None,dry_run,Some(&[]),&requested.refs,
                &|path|Ok(if path.starts_with(&unrelated.root.path) && !unrelated.deps.exists() {65536} else {0})).unwrap();
            assert!(unrelated.deps.exists(),"reclaimed warm cache on a filesystem unused by this admission");
            assert!(!reports.iter().any(|r|r.path.starts_with(&unrelated.root.path.to_string_lossy().into_owned()) && r.removed>0),
                "cleanup reported unrelated filesystem reclamation");
            assert!(reports.iter().any(|r|r.path.starts_with(&requested.root.path.to_string_lossy().into_owned()) && r.removed>0));
            assert_eq!(requested.deps.exists(),dry_run);
        }
    }

    #[test]
    fn warm_release_cache_restores_headroom_without_removing_binary_or_unknown_files() {
        let f = Fixture::new();
        let reports = f.run(false, None, Some(&[]), &f.refs);
        assert!(
            !f.deps.exists(),
            "warm rebuildable artifacts must unblock admission"
        );
        assert_eq!(std::fs::read(&f.binary).unwrap(), b"live executable");
        assert_eq!(
            std::fs::read(f.root.path.join("release/unknown")).unwrap(),
            b"preserve"
        );
        assert!(reports.iter().any(|r| r.freed_bytes > 0));
        assert!(f
            .run(false, None, Some(&[]), &f.refs)
            .iter()
            .all(|r| r.removed == 0));
    }

    #[test]
    fn pressure_preserves_held_leases_and_unavailable_or_live_process_evidence() {
        let f = Fixture::new();
        let lease = TargetLeases::try_cache_roots(std::slice::from_ref(&f.root.path), None)
            .unwrap()
            .unwrap();
        assert!(f
            .run(false, None, Some(&[]), &f.refs)
            .iter()
            .any(|r| r.outcome.contains("lease")));
        assert!(f.deps.exists());
        drop(lease);
        f.run(false, None, None, &f.refs);
        assert!(f.deps.exists());
        f.run(
            false,
            None,
            Some(&[BuildProc {
                target_env: Some(f.root.path.clone()),
                ..Default::default()
            }]),
            &f.refs,
        );
        assert!(f.deps.exists());
        f.run(false, None, Some(&[]), &References::default());
        assert!(f.deps.exists());
        let refs = References {
            reliable: true,
            paths: vec![f.deps.join("warm.rlib")],
            ..Default::default()
        };
        f.run(false, None, Some(&[]), &refs);
        assert!(f.deps.exists());
    }

    #[test]
    fn pressure_dry_run_and_symlinks_preserve_data() {
        let f = Fixture::new();
        let external = f._dir.path().join("state-and-snapshots");
        std::fs::create_dir(&external).unwrap();
        std::fs::write(external.join("data.db"), b"private").unwrap();
        symlink(&external, f.root.path.join("release/build")).unwrap();
        let reports = f.run(true, None, Some(&[]), &f.refs);
        assert!(reports.iter().any(|r| r.removed > 0));
        assert!(f.deps.exists());
        f.run(false, None, Some(&[]), &f.refs);
        assert_eq!(std::fs::read(external.join("data.db")).unwrap(), b"private");
        assert!(f
            .root
            .path
            .join("release/build")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn pressure_only_reclaims_profiles_with_a_measured_deficit() {
        let f = Fixture::new();
        let other = Fixture::new();
        reclaim_roots(
            &[f.root.clone(), other.root.clone()],
            65536,
            None,
            false,
            Some(&[]),
            &f.refs,
            &|p| {
                Ok(if p.starts_with(&other.root.path) || !f.deps.exists() {
                    65536
                } else {
                    0
                })
            },
        )
        .unwrap();
        assert!(!f.deps.exists());
        assert!(other.deps.exists());
    }

    #[test]
    fn pressure_stops_when_incremental_is_enough_and_reports_insufficient_space() {
        let f = Fixture::new();
        let incremental = f.root.path.join("release/incremental");
        std::fs::create_dir(&incremental).unwrap();
        std::fs::write(incremental.join("cache"), vec![1; 65536]).unwrap();
        reclaim_roots(
            std::slice::from_ref(&f.root),
            65536,
            None,
            false,
            Some(&[]),
            &f.refs,
            &|_| Ok(if incremental.exists() { 0 } else { 65536 }),
        )
        .unwrap();
        assert!(!incremental.exists());
        assert!(f.deps.exists(), "stop as soon as measured headroom fits");
        let reports = reclaim_roots(
            std::slice::from_ref(&f.root),
            65536,
            None,
            false,
            Some(&[]),
            &f.refs,
            &|_| Ok(0),
        )
        .unwrap();
        assert!(reports
            .iter()
            .any(|r| r.outcome.contains("remaining deficit 65536 bytes")));
        assert!(f.binary.exists());
    }

    #[test]
    fn updater_can_reclaim_under_its_own_release_lease() {
        let f = Fixture::new();
        let lease = TargetLeases::try_release_root(&f.root.path)
            .unwrap()
            .unwrap();
        f.run(false, Some(&lease), Some(&[]), &f.refs);
        assert!(!f.deps.exists());
        assert!(f.binary.exists());
    }

    #[test]
    fn missing_cache_evidence_and_source_roots_are_not_reclaimed() {
        let f = Fixture::new();
        std::fs::remove_file(f.root.path.join("CACHEDIR.TAG")).unwrap();
        f.run(false, None, Some(&[]), &f.refs);
        assert!(f.deps.exists());
        std::fs::write(
            f.root.path.join("CACHEDIR.TAG"),
            "Signature: 8a477f597d28d172789f06886806bc55\n",
        )
        .unwrap();
        std::fs::write(f.root.path.join("Cargo.toml"), "[package]").unwrap();
        f.run(false, None, Some(&[]), &f.refs);
        assert!(f.deps.exists());
    }

    #[test]
    fn dry_run_carries_estimated_headroom_between_caches_on_the_same_filesystem() {
        let first = Fixture::new();
        let second = Fixture::new();
        let reports = reclaim_roots(
            &[first.root.clone(), second.root.clone()],
            65536,
            None,
            true,
            Some(&[]),
            &first.refs,
            &|_| Ok(0),
        )
        .unwrap();
        assert_eq!(
            reports.len(),
            1,
            "the first cache already supplies the estimated deficit"
        );
        assert!(first.deps.exists());
        assert!(second.deps.exists());
    }
}
