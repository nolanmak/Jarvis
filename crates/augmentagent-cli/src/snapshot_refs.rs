//! Conservative references for retention. Unknown process or service state
//! cannot authorize deleting a rollback point or a rebuildable cache.
//!
//! One policy serves both (#1753). Non-dumpable login/key agents (`sshd`,
//! `ssh-agent`, `(sd-pam)`) make the kernel deny `exe`, `cwd` and `fd` even to
//! their own uid, and every login session has them; treating that permission
//! wall as unknown evidence froze rollback retention permanently. Their
//! `status` and `cmdline` stay readable and still count: a command line that
//! names a path protects it. Any other unreadable evidence is still unknown.
use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct References {
    pub paths: Vec<PathBuf>,
    pub config: String,
    pub reliable: bool,
    /// The first piece of evidence that could not be read, for the operator.
    pub unavailable: Option<String>,
    /// How many further pieces of evidence were also unreadable.
    pub unavailable_more: usize,
}

/// Why retention keeps a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Protection {
    Pinned,
    Referenced,
    ProbeUnavailable(String),
}

impl std::fmt::Display for Protection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Protection::Pinned => f.write_str("pinned"),
            Protection::Referenced => f.write_str("referenced"),
            Protection::ProbeUnavailable(why) => write!(f, "reference probe unavailable ({why})"),
        }
    }
}

impl References {
    pub fn protects(&self, path: &Path) -> bool {
        self.retention_reason(path).is_some()
    }

    /// Why `path` must be kept, or `None` when nothing protects it. A pin
    /// outranks a reference, which outranks missing evidence, so the reason
    /// shown is the one the operator can act on.
    pub fn retention_reason(&self, path: &Path) -> Option<Protection> {
        if path.join(".pin").exists() {
            return Some(Protection::Pinned);
        }
        let canonical = std::fs::canonicalize(path).ok();
        let matches = |candidate: &Path| self.paths.iter().any(|p| p == candidate || p.starts_with(candidate))
            || self.config.contains(candidate.to_string_lossy().as_ref());
        if matches(path) || canonical.as_deref().is_some_and(matches) {
            return Some(Protection::Referenced);
        }
        (!self.reliable).then(|| Protection::ProbeUnavailable(self.unavailable_summary()))
    }

    /// The first unreadable piece of evidence, plus how many others there were.
    pub fn unavailable_summary(&self) -> String {
        let first = self.unavailable.as_deref().unwrap_or("evidence unreadable");
        if self.unavailable_more == 0 { first.to_string() } else { format!("{first}; +{} more", self.unavailable_more) }
    }

    fn distrust(&mut self, why: impl FnOnce() -> String) {
        self.reliable = false;
        if self.unavailable.is_none() {
            self.unavailable = Some(why());
        } else {
            self.unavailable_more += 1;
        }
    }
}

pub fn collect() -> References {
    collect_processes_and_units()
}

/// The same evidence as [`collect`]. Rebuildable caches and clean merged
/// worktrees additionally require compiler inventory and writer leases.
pub fn collect_rebuildable() -> References {
    collect()
}

/// What `/proc/<pid>` revealed about one live, same-uid process.
pub struct ProcEvidence {
    pub pid: u32,
    pub comm: String,
    pub exe: Result<PathBuf, std::io::ErrorKind>,
    pub cwd: Result<PathBuf, std::io::ErrorKind>,
    /// `None` when the command line could not be read.
    pub cmdline: Option<String>,
    pub fds: Result<Vec<PathBuf>, std::io::ErrorKind>,
}

/// A handle the kernel hides from a non-dumpable process is not unknown
/// evidence; any other failure on a live process is.
fn unknown_handle_error(error: std::io::ErrorKind) -> bool {
    error != std::io::ErrorKind::PermissionDenied
}

/// Fold one live process's evidence into `out` (policy table in #1753).
pub fn absorb(out: &mut References, ev: ProcEvidence) {
    let who = |handle: &str, kind: Option<std::io::ErrorKind>| match kind {
        Some(k) => format!("pid {} {}: {handle} {k:?}", ev.pid, ev.comm),
        None => format!("pid {} {}: {handle} unreadable", ev.pid, ev.comm),
    };
    match &ev.exe {
        Ok(exe) => out.paths.push(exe.clone()),
        Err(e) if unknown_handle_error(*e) => out.distrust(|| who("exe", Some(*e))),
        Err(_) => {}
    }
    match &ev.cwd {
        Ok(cwd) => out.paths.push(cwd.clone()),
        Err(e) if unknown_handle_error(*e) => out.distrust(|| who("cwd", Some(*e))),
        Err(_) => {}
    }
    // Command arguments include script paths and configured rollback dirs.
    match &ev.cmdline {
        Some(args) => {
            out.config.push_str(args);
            out.config.push('\n');
        }
        None => out.distrust(|| who("cmdline", None)),
    }
    match ev.fds {
        Ok(fds) => out.paths.extend(fds),
        Err(e) if unknown_handle_error(e) => out.distrust(|| who("fd", Some(e))),
        Err(_) => {}
    }
}

fn read_evidence(p: &Path, pid: u32) -> Option<ProcEvidence> {
    // A process that exits mid-read is no evidence either way.
    let gone = || !p.exists();
    let kind = |e: std::io::Error| e.kind();
    let exe = std::fs::read_link(p.join("exe"))
        .map(|exe| PathBuf::from(exe.to_string_lossy().trim_end_matches(" (deleted)")))
        .map_err(kind);
    let cwd = std::fs::read_link(p.join("cwd")).map_err(kind);
    let cmdline = std::fs::read(p.join("cmdline")).ok().map(|a| String::from_utf8_lossy(&a).into_owned());
    let fds = std::fs::read_dir(p.join("fd"))
        .map(|fds| fds.flatten().filter_map(|fd| std::fs::read_link(fd.path()).ok()).collect())
        .map_err(kind);
    if (exe.is_err() || cwd.is_err() || cmdline.is_none() || fds.is_err()) && gone() {
        return None;
    }
    let comm = std::fs::read_to_string(p.join("comm")).map(|c| c.trim().to_string()).unwrap_or_default();
    Some(ProcEvidence { pid, comm, exe, cwd, cmdline, fds })
}

fn collect_processes_and_units() -> References {
    use std::os::unix::fs::MetadataExt;
    let mut out = References {
        reliable: true,
        ..Default::default()
    };
    let Ok(procs) = std::fs::read_dir("/proc") else {
        return References::default();
    };
    for entry in procs {
        let Ok(entry) = entry else {
            out.distrust(|| "/proc listing incomplete".into());
            continue;
        };
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        let p = entry.path();
        let Ok(meta) = std::fs::metadata(&p) else {
            continue;
        };
        if meta.uid() != unsafe { libc::geteuid() } {
            continue;
        }
        let Ok(status) = std::fs::read_to_string(p.join("status")) else {
            if p.exists() {
                out.distrust(|| format!("pid {pid}: status unreadable"));
            }
            continue;
        };
        if status
            .lines()
            .any(|s| s.starts_with("State:") && (s.contains("Z (") || s.contains("X (")))
        {
            continue;
        }
        if let Some(ev) = read_evidence(&p, pid) {
            absorb(&mut out, ev);
        }
    }
    // Include the effective unit and all user unit/drop-in files, including
    // dormant rollback references which do not appear in a running argv.
    let unit = std::process::Command::new("systemctl")
        .args(["--user", "cat", "augmentagent.service"])
        .output();
    match unit {
        Ok(v) if v.status.success() => out.config.push_str(&String::from_utf8_lossy(&v.stdout)),
        _ => out.distrust(|| "systemctl --user cat augmentagent.service failed".into()),
    }
    if let Some(home) = std::env::var_os("HOME") {
        read_config_tree(&PathBuf::from(home).join(".config/systemd/user"), &mut out);
    }
    out
}

fn read_config_tree(path: &Path, out: &mut References) {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                out.distrust(|| format!("{}: {error}", path.display()));
            }
            return;
        }
    };
    if meta.is_dir() {
        match std::fs::read_dir(path) {
            Ok(entries) => {
                for entry in entries {
                    match entry {
                        Ok(e) => read_config_tree(&e.path(), out),
                        Err(_) => out.distrust(|| format!("{}: entry unreadable", path.display())),
                    }
                }
            }
            Err(_) => out.distrust(|| format!("{}: unreadable", path.display())),
        }
    } else if (meta.is_file() || meta.file_type().is_symlink())
        && matches!(
            path.extension().and_then(|s| s.to_str()),
            Some("service" | "conf")
        )
    {
        match std::fs::read_to_string(path) {
            Ok(s) => {
                out.config.push_str(&s);
                out.config.push('\n');
            }
            Err(_) => out.distrust(|| format!("{}: unreadable", path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::ErrorKind::{NotFound, Other, PermissionDenied};

    fn ev(exe: Result<&str, std::io::ErrorKind>, cwd: Result<&str, std::io::ErrorKind>,
        cmdline: Option<&str>, fds: Result<Vec<&str>, std::io::ErrorKind>) -> ProcEvidence {
        ProcEvidence {
            pid: 42,
            comm: "agent".into(),
            exe: exe.map(PathBuf::from),
            cwd: cwd.map(PathBuf::from),
            cmdline: cmdline.map(str::to_string),
            fds: fds.map(|v| v.into_iter().map(PathBuf::from).collect()),
        }
    }

    fn folded(e: ProcEvidence) -> References {
        let mut refs = References { reliable: true, ..Default::default() };
        absorb(&mut refs, e);
        refs
    }

    // #1753 D2: non-dumpable login/key agents (sshd, ssh-agent, sd-pam) hide
    // exe, cwd and fd together; they must not freeze rollback retention.
    #[test]
    fn a_non_dumpable_agent_hiding_every_handle_leaves_the_probe_reliable() {
        let refs = folded(ev(Err(PermissionDenied), Err(PermissionDenied), Some("sshd: owner@notty"), Err(PermissionDenied)));
        assert!(refs.reliable, "{:?}", refs.unavailable);
        assert!(refs.retention_reason(Path::new("/fixture/snapshots/old")).is_none());
    }

    #[test]
    fn each_hidden_handle_alone_is_tolerated() {
        for e in [
            ev(Err(PermissionDenied), Ok("/w"), Some("a"), Ok(vec![])),
            ev(Ok("/bin/a"), Err(PermissionDenied), Some("a"), Ok(vec![])),
            ev(Ok("/bin/a"), Ok("/w"), Some("a"), Err(PermissionDenied)),
        ] {
            assert!(folded(e).reliable);
        }
    }

    #[test]
    fn a_hidden_process_naming_a_snapshot_protects_only_that_snapshot() {
        let refs = folded(ev(Err(PermissionDenied), Err(PermissionDenied),
            Some("sqlite3 /fixture/snapshots/old/data.db.gz"), Err(PermissionDenied)));
        assert!(refs.reliable);
        assert_eq!(refs.retention_reason(Path::new("/fixture/snapshots/old")), Some(Protection::Referenced));
        assert_eq!(refs.retention_reason(Path::new("/fixture/snapshots/older")), None);
    }

    #[test]
    fn evidence_other_than_a_permission_wall_still_makes_the_probe_unreliable() {
        for (e, handle) in [
            (ev(Err(Other), Ok("/w"), Some("a"), Ok(vec![])), "exe"),
            (ev(Ok("/bin/a"), Err(Other), Some("a"), Ok(vec![])), "cwd"),
            (ev(Ok("/bin/a"), Ok("/w"), Some("a"), Err(Other)), "fd"),
            (ev(Ok("/bin/a"), Ok("/w"), None, Ok(vec![])), "cmdline"),
        ] {
            let refs = folded(e);
            assert!(!refs.reliable, "{handle}");
            let why = refs.retention_reason(Path::new("/fixture/x")).unwrap().to_string();
            assert!(why.starts_with("reference probe unavailable (pid 42 agent: ") && why.contains(handle), "{why}");
        }
    }

    #[test]
    fn the_first_blocker_is_named_and_the_rest_are_counted() {
        let mut refs = References { reliable: true, ..Default::default() };
        absorb(&mut refs, ev(Err(Other), Err(Other), None, Ok(vec![])));
        assert_eq!(refs.unavailable.as_deref(), Some("pid 42 agent: exe Other"));
        assert_eq!(refs.unavailable_summary(), "pid 42 agent: exe Other; +2 more");
        let _ = NotFound;
    }

    #[test]
    fn visible_handles_still_protect_what_they_reference() {
        let refs = folded(ev(Ok("/fixture/snapshots/old/augmentagent"), Ok("/w"), Some("x"), Ok(vec!["/fixture/snapshots/new/data.db.gz"])));
        assert_eq!(refs.retention_reason(Path::new("/fixture/snapshots/old")), Some(Protection::Referenced));
        assert_eq!(refs.retention_reason(Path::new("/fixture/snapshots/new")), Some(Protection::Referenced));
    }

    #[test]
    fn hidden_login_agent_handles_freeze_neither_rollbacks_nor_rebuildable_caches() {
        assert!(!unknown_handle_error(PermissionDenied));
        assert!(unknown_handle_error(Other), "unexpected evidence failure must still retain everything");
        let refs = folded(ev(Err(PermissionDenied), Err(PermissionDenied), Some("(sd-pam)"), Err(PermissionDenied)));
        assert!(!refs.protects(Path::new("/fixture/target/release/deps")));
        assert!(!refs.protects(Path::new("/fixture/deploy-snapshots/old")));
    }

    #[test]
    fn canonical_process_references_protect_symlinked_and_relative_candidates() {
        use std::os::unix::fs::symlink;
        let t = tempfile::tempdir().unwrap();
        let snapshot = t.path().join("snapshot");
        std::fs::create_dir(&snapshot).unwrap();
        symlink(&snapshot, t.path().join("alias")).unwrap();
        let canonical = std::fs::canonicalize(&snapshot).unwrap();
        let refs = References { reliable: true, paths: vec![canonical.join("held.db")], ..Default::default() };
        assert!(refs.protects(&t.path().join("alias")));
        let mut relative = PathBuf::new();
        for _ in std::env::current_dir().unwrap().components().skip(1) { relative.push(".."); }
        relative.push(canonical.strip_prefix("/").unwrap());
        assert!(refs.protects(&relative));
    }

    #[test]
    fn symlinked_units_and_dropins_preserve_rollback_references_without_following_directory_loops()
    {
        use std::os::unix::fs::symlink;
        let t = tempfile::tempdir().unwrap();
        let units = t.path().join("units");
        std::fs::create_dir(&units).unwrap();
        let rollback = t.path().join("rollback");
        let external = t.path().join("external-unit");
        std::fs::write(
            &external,
            format!("ExecStart={}/augmentagent\n", rollback.display()),
        )
        .unwrap();
        symlink(&external, units.join("agent.service")).unwrap();
        symlink(&external, units.join("override.conf")).unwrap();
        symlink(&units, units.join("loop")).unwrap();
        let mut references = References {
            reliable: true,
            ..Default::default()
        };
        read_config_tree(&units, &mut references);
        assert!(references.reliable);
        assert!(references.protects(&rollback));
    }

    #[test]
    fn unreadable_or_dangling_unit_links_cannot_authorize_retention() {
        use std::os::unix::fs::symlink;
        let t = tempfile::tempdir().unwrap();
        symlink(t.path().join("missing"), t.path().join("agent.service")).unwrap();
        let mut references = References {
            reliable: true,
            ..Default::default()
        };
        read_config_tree(t.path(), &mut references);
        assert!(!references.reliable);
        assert!(references.protects(&t.path().join("rollback")));
    }
}
