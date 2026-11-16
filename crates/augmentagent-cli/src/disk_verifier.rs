//! Count and byte retention for independently owned verifier targets.
//!
//! Per parent, AUGMENTAGENT_PRUNE_VERIFIER_KEEP (default 3) and
//! AUGMENTAGENT_PRUNE_VERIFIER_MAX_GB (default 20 GiB) both bound retention.
//! Latest recursive writes order eviction, regardless of age. Protected caches
//! count toward both bounds but can leave them exceeded; they are deferred.
use super::*;
use crate::storage_leases::TargetLeases;

#[derive(Clone, Copy)]
struct Bounds {
    keep: usize,
    max_bytes: u64,
}
impl Bounds {
    fn parse(keep: Option<&str>, gb: Option<&str>) -> Self {
        let keep = keep.and_then(|v| v.trim().parse().ok()).unwrap_or(3);
        let gb = gb
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|v| v.is_finite() && *v >= 0.0)
            .unwrap_or(20.0);
        Self {
            keep,
            max_bytes: (gb * GIB) as u64,
        }
    }
    fn from_env() -> Self {
        Self::parse(
            std::env::var("AUGMENTAGENT_PRUNE_VERIFIER_KEEP")
                .ok()
                .as_deref(),
            std::env::var("AUGMENTAGENT_PRUNE_VERIFIER_MAX_GB")
                .ok()
                .as_deref(),
        )
    }
}

pub(super) fn verifier_name(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("augmentagent-verifier-"))
}

/// Called under the prune/build exclusion. Ownership discovery deliberately
/// excludes name-only matches and replaced roots; no sibling glob grants
/// authority. Explicit --root trims retain their existing per-target meaning.
pub(super) fn prune_discovered(
    repo: &Path,
    dry: bool,
    procs: Option<&[BuildProc]>,
    held: Option<&TargetLeases>,
) -> Result<Vec<RootReport>> {
    let state = crate::build_storage::state_dir()?;
    Ok(prune(repo, &state, dry, procs, held, Bounds::from_env()))
}

fn prune(
    repo: &Path,
    state: &Path,
    dry: bool,
    procs: Option<&[BuildProc]>,
    held: Option<&TargetLeases>,
    bounds: Bounds,
) -> Vec<RootReport> {
    let mut groups = std::collections::BTreeMap::<PathBuf, Vec<PathBuf>>::new();
    for target in crate::build_storage::registered(repo, state) {
        if verifier_name(&target.path) {
            if let Some(parent) = target.path.parent() {
                groups
                    .entry(parent.to_path_buf())
                    .or_default()
                    .push(target.path);
            }
        }
    }
    let mut reports = Vec::new();
    for paths in groups.values_mut() {
        paths.sort();
        paths.dedup();
        let mut caches: Vec<_> = paths
            .iter()
            .map(|path| {
                (
                    path,
                    crate::build_storage::newest_write(path),
                    disk_bytes(path),
                )
            })
            .collect();
        caches.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(b.0)));
        let mut count = caches.len();
        let mut bytes = caches
            .iter()
            .fold(0u64, |total, c| total.saturating_add(c.2));
        for (path, newest, size) in caches {
            let root = Root::at("verifier cache", path);
            let mut report = RootReport {
                label: root.label.clone(),
                path: path.display().to_string(),
                outcome: "kept: within verifier count and byte bounds".into(),
                removed: 0,
                freed_bytes: 0,
                full_drop: false,
            };
            // Even an admission pruning inside its own writer's lease cannot
            // retire that writer's verifier cache.
            let own_lease = held.is_some_and(|leases| {
                leases.owns_directory(path)
                    || root.profiles.iter().any(|p| leases.owns_directory(p))
            });
            let lease =
                TargetLeases::try_cache_roots(std::slice::from_ref(path), held, &profile_dirs);
            let reason = if own_lease || matches!(lease, Ok(None)) {
                Some("a managed build owns this target lease".to_string())
            } else if let Err(error) = &lease {
                Some(format!("cannot verify target lease: {error:#}"))
            } else if path.canonicalize().ok().as_ref() != Some(path)
                || !path.symlink_metadata().is_ok_and(|m| m.is_dir())
                || root.profiles.iter().any(|p| !p.starts_with(path))
            {
                Some("target boundary changed or external profile".to_string())
            } else if newest.is_none() {
                Some("cannot measure latest verifier write".to_string())
            } else if procs.is_none_or(|procs| {
                root_in_use(path, procs, &|p| p.canonicalize().ok())
                    || root
                        .profiles
                        .iter()
                        .any(|p| root_in_use(p, procs, &|p| p.canonicalize().ok()))
            }) {
                Some("active writer or unavailable build-process evidence".to_string())
            } else {
                None
            };
            if let Some(reason) = reason {
                report.outcome = format!("deferred: {reason}");
            } else if count > bounds.keep || bytes > bounds.max_bytes {
                // Revalidate the ownership record while holding the physical
                // target/profile leases, immediately before whole-root removal.
                if !crate::build_storage::registered(repo, state)
                    .iter()
                    .any(|t| t.path == *path)
                {
                    report.outcome = "deferred: verifier ownership changed".into();
                } else if dry || remove(path) {
                    count -= 1;
                    bytes = bytes.saturating_sub(size);
                    report.removed = 1;
                    report.freed_bytes = size;
                    report.full_drop = true;
                    report.outcome = format!(
                        "{} oldest idle verifier cache (count or byte bound)",
                        if dry { "would remove" } else { "removed" }
                    );
                } else {
                    report.outcome = "failed to remove verifier cache".into();
                }
            }
            // `lease` stays alive through removal (or the dry-run decision).
            drop(lease);
            reports.push(report);
        }
    }
    reports
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Fixture {
        _dir: tempfile::TempDir,
        repo: PathBuf,
        state: PathBuf,
        parent: PathBuf,
        caches: Vec<PathBuf>,
    }
    impl Fixture {
        fn new(count: usize) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let base = dir.path().canonicalize().unwrap();
            let repo = base.join("repo");
            fs::create_dir(&repo).unwrap();
            assert!(std::process::Command::new("git")
                .args(["init", "-q"])
                .arg(&repo)
                .status()
                .unwrap()
                .success());
            let state = base.join("state");
            let parent = base.join("caches");
            fs::create_dir(&parent).unwrap();
            let caches = (0..count)
                .map(|i| {
                    let path = parent.join(format!("augmentagent-verifier-{i:064x}"));
                    crate::build_storage::register(&repo, &path, Some(&repo), &state).unwrap();
                    fs::create_dir_all(path.join("debug/deps")).unwrap();
                    fs::write(path.join("debug/deps/warm.rlib"), vec![7; 1024 * 1024]).unwrap();
                    fn age_tree(path: &Path, time: SystemTime) {
                        if path.is_dir() {
                            for entry in fs::read_dir(path).unwrap() {
                                age_tree(&entry.unwrap().path(), time);
                            }
                        }
                        fs::File::open(path)
                            .unwrap()
                            .set_times(fs::FileTimes::new().set_modified(time))
                            .unwrap();
                    }
                    age_tree(
                        &path,
                        SystemTime::now() - Duration::from_secs(3500 - i as u64 * 300),
                    );
                    path
                })
                .collect();
            Self {
                _dir: dir,
                repo,
                state,
                parent,
                caches,
            }
        }
        fn run(&self, dry: bool, bounds: Bounds, procs: Option<&[BuildProc]>) -> Vec<RootReport> {
            prune(&self.repo, &self.state, dry, procs, None, bounds)
        }
    }
    fn bounds(keep: usize) -> Bounds {
        Bounds {
            keep,
            max_bytes: u64::MAX,
        }
    }
    fn removed(reports: &[RootReport]) -> usize {
        reports.iter().map(|r| r.removed).sum()
    }

    #[test]
    fn young_verifiers_keep_three_newest_and_dry_run_matches_bytes() {
        let f = Fixture::new(6);
        let expected: u64 = f.caches[..3].iter().map(|p| disk_bytes(p)).sum();
        let dry = f.run(true, bounds(3), Some(&[]));
        assert!(f.caches.iter().all(|p| p.exists()));
        assert_eq!(
            removed(&dry),
            3,
            "young caches must be bounded regardless of age"
        );
        let real = f.run(false, bounds(3), Some(&[]));
        assert_eq!(
            dry.iter()
                .map(|r| (&r.path, r.removed, r.freed_bytes))
                .collect::<Vec<_>>(),
            real.iter()
                .map(|r| (&r.path, r.removed, r.freed_bytes))
                .collect::<Vec<_>>()
        );
        let freed: u64 = real.iter().map(|r| r.freed_bytes).sum();
        assert!(freed.abs_diff(expected) <= 4096);
        for (i, path) in f.caches.iter().enumerate() {
            assert_eq!(path.exists(), i >= 3);
        }
    }

    #[test]
    fn size_cap_removes_oldest_until_two_fit_and_preserves_leased_newest() {
        let f = Fixture::new(6);
        let cap: u64 = f.caches[4..].iter().map(|p| disk_bytes(p)).sum();
        let limit = Bounds::parse(Some("99"), Some(&(cap as f64 / GIB).to_string()));
        let lease = TargetLeases::try_cache_roots(&f.caches[5..], None, &profile_dirs)
            .unwrap()
            .unwrap();
        let dry = f.run(true, limit, Some(&[]));
        assert_eq!(removed(&dry), 4);
        assert!(f.caches.iter().all(|p| p.exists()));
        let real = f.run(false, limit, Some(&[]));
        assert_eq!(removed(&real), 4);
        assert!(f.caches[4..].iter().all(|p| p.exists()));
        assert!(f.caches.iter().map(|p| disk_bytes(p)).sum::<u64>() <= cap);
        // Even a cap smaller than the leased cache cannot remove it.
        let real = f.run(
            false,
            Bounds {
                keep: 99,
                max_bytes: 0,
            },
            Some(&[]),
        );
        assert!(f.caches[5].exists());
        assert!(real
            .iter()
            .any(|r| r.path == f.caches[5].display().to_string()
                && r.outcome.starts_with("deferred")));
        drop(lease);
    }

    #[test]
    fn oldest_leased_and_active_writer_verifiers_are_deferred() {
        let f = Fixture::new(6);
        let lease = TargetLeases::try_cache_roots(&f.caches[..1], None, &profile_dirs)
            .unwrap()
            .unwrap();
        let procs = [BuildProc {
            target_env: Some(f.caches[1].clone()),
            ..Default::default()
        }];
        let dry = f.run(true, bounds(3), Some(&procs));
        let real = f.run(false, bounds(3), Some(&procs));
        assert_eq!(removed(&dry), removed(&real));
        assert_eq!(removed(&real), 3);
        assert!(f.caches[..2].iter().all(|p| p.exists()));
        for p in &f.caches[..2] {
            assert!(real
                .iter()
                .any(|r| r.path == p.display().to_string() && r.outcome.starts_with("deferred")));
        }
        drop(lease);
    }

    #[test]
    fn unknown_and_symlinked_verifiers_and_their_destinations_survive() {
        let f = Fixture::new(3);
        let unknown = f
            .parent
            .join(format!("augmentagent-verifier-{}", "a".repeat(64)));
        fs::create_dir(&unknown).unwrap();
        fs::write(unknown.join("private"), b"keep").unwrap();
        let link = f
            .parent
            .join(format!("augmentagent-verifier-{}", "b".repeat(64)));
        std::os::unix::fs::symlink(&unknown, &link).unwrap();
        // Replace a previously owned target with a link too.
        fs::remove_dir_all(&f.caches[0]).unwrap();
        std::os::unix::fs::symlink(&unknown, &f.caches[0]).unwrap();
        let dry = f.run(true, bounds(0), Some(&[]));
        assert!(f.caches.iter().all(|p| p.exists()));
        let real = f.run(false, bounds(0), Some(&[]));
        assert_eq!(removed(&dry), removed(&real));
        assert_eq!(removed(&real), 2);
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert!(f.caches[0]
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read(unknown.join("private")).unwrap(), b"keep");
    }

    #[test]
    fn unavailable_process_evidence_defers_all_verifiers() {
        let f = Fixture::new(4);
        let reports = f.run(false, bounds(0), None);
        assert_eq!(removed(&reports), 0);
        assert!(reports.iter().all(|r| r.outcome.starts_with("deferred")));
        assert!(f.caches.iter().all(|p| p.exists()));
    }

    #[test]
    fn verifier_bounds_are_per_parent_and_keep_old_caches_within_bounds() {
        let f = Fixture::new(6);
        let second = f.parent.with_file_name("second-parent");
        let other = second.join(format!("augmentagent-verifier-{:064x}", 0));
        crate::build_storage::register(&f.repo, &other, Some(&f.repo), &f.state).unwrap();
        fs::create_dir_all(other.join("debug/deps")).unwrap();
        let artifact = other.join("debug/deps/old.rlib");
        fs::write(&artifact, vec![1; 1024 * 1024]).unwrap();
        let old = SystemTime::now() - Duration::from_secs(90 * 86400);
        fs::File::open(&artifact)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(old))
            .unwrap();
        let reports = f.run(false, bounds(3), Some(&[]));
        assert_eq!(removed(&reports), 3);
        assert!(
            artifact.exists(),
            "another parent's caches must have independent bounds"
        );
        assert!(f.caches[3..].iter().all(|p| p.exists()));
    }

    #[test]
    fn nested_recent_write_counts_as_use_even_with_old_root_mtime() {
        let f = Fixture::new(4);
        fs::write(
            f.caches[0].join("debug/deps/warm.rlib"),
            vec![8; 1024 * 1024],
        )
        .unwrap();
        f.run(false, bounds(3), Some(&[]));
        assert!(f.caches[0].exists());
        assert!(
            !f.caches[1].exists(),
            "root mtime alone must not decide recency"
        );
    }

    #[test]
    fn admission_cannot_remove_its_own_held_verifier_cache() {
        let f = Fixture::new(1);
        let held = TargetLeases::try_cache_roots(&f.caches, None, &profile_dirs)
            .unwrap()
            .unwrap();
        let reports = prune(&f.repo, &f.state, false, Some(&[]), Some(&held), bounds(0));
        assert_eq!(removed(&reports), 0);
        assert!(reports[0].outcome.starts_with("deferred"));
        assert!(f.caches[0].exists());
    }

    #[test]
    fn name_only_verifiers_are_never_automatically_adopted() {
        for key in ["a".repeat(64), "unknown-format".into(), String::new()] {
            let unknown = crate::build_storage::UnmanagedCache {
                path: PathBuf::from(format!("/build/augmentagent-verifier-{key}")),
                bytes: 1024,
                idle_days: Some(90.0),
            };
            assert!(stale_unmanaged(&[unknown], 7.0, |_| false).is_empty());
        }
    }

    #[test]
    fn verifier_bounds_defaults_and_invalid_values() {
        let default = Bounds::parse(None, None);
        assert_eq!(default.keep, 3);
        assert_eq!(default.max_bytes, 20 * GIB as u64);
        for value in ["-1", "NaN", "inf", "bad"] {
            let b = Bounds::parse(Some(value), Some(value));
            assert_eq!(b.keep, 3);
            assert_eq!(b.max_bytes, default.max_bytes);
        }
        assert_eq!(Bounds::parse(Some("0"), Some("0")).max_bytes, 0);
    }
}
