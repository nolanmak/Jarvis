//! Disk hygiene for build output (#1406): `augmentagent disk prune|status`.
//!
//! Cargo never deletes an artifact, so every target dir this project writes
//! grows until the filesystem is full. This module bounds them without
//! slowing a build that is warm:
//!
//! * **What is cold** is decided per cargo *unit* (the 16-hex hash shared by
//!   `.fingerprint/<pkg>-<hash>/`, `deps/*-<hash>.*` and `build/<pkg>-<hash>/`),
//!   by mark-and-sweep over cargo's own dependency records: a unit is in use
//!   as of the newest compile of anything that depends on it. Deleting by the
//!   artifact's own age instead would drop a third-party rlib compiled a month
//!   ago and still linked today, and cascade into a full rebuild. Access
//!   times are deliberately not used: build volumes are commonly `noatime`.
//! * **Unknown format means hands off.** If the fingerprint records stop
//!   resolving (a cargo change), artifacts are kept and only `incremental/`
//!   is pruned.
//! * **Never under a build.** A root a running cargo/rustc is using, or the
//!   gate cache while a lane holds its lock, is skipped whole.
//!
//! Selection is pure over [`Candidate`]s; the filesystem walk and the process
//! probe are thin and injected, so no test builds a real target dir.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::Result;

const DAY: f64 = 86_400.0;

/// Free bytes below which a gate refuses to start (#1407):
/// `AUGMENTAGENT_GATE_MIN_FREE_GB`, default 15 — a workspace test build
/// needs well over that.
pub const DEFAULT_GATE_MIN_FREE_GB: f64 = 15.0;
/// Phrase [`floor_refusal`] leads with; `infra_failure_reason` keys on it so
/// a refusal is billed to the box, not the builder.
pub const LOW_DISK_REFUSAL: &str = "gate refused: low disk";

pub fn gate_min_free_gb() -> f64 {
    env_f64("AUGMENTAGENT_GATE_MIN_FREE_GB", DEFAULT_GATE_MIN_FREE_GB)
}

fn env_f64(key: &str, default: f64) -> f64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v >= 0.0)
        .unwrap_or(default)
}

/// `Some(reason)` when `free_bytes` is under the floor. Pure, so the
/// threshold is testable without filling a disk.
pub fn floor_refusal(free_bytes: u64, floor_gb: f64) -> Option<String> {
    let free_gb = free_bytes as f64 / GIB;
    (free_gb < floor_gb).then(|| {
        format!(
            "{LOW_DISK_REFUSAL} ({free_gb:.1} GB free < {floor_gb:.0} GB floor on the gate \
             target's filesystem). Free space or run `augmentagent disk prune`."
        )
    })
}

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// Refusal for a gate about to build into `target_dir`, if its filesystem is
/// under the floor. An unreadable filesystem never refuses.
pub fn gate_floor_refusal(target_dir: &Path) -> Option<String> {
    floor_refusal(available_bytes(target_dir)?, gate_min_free_gb())
}

/// Available bytes on the volume holding `path`, resolved through its nearest
/// existing ancestor (a dir not created yet sits on its parent's volume).
pub fn available_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let dir = nearest_existing(path)?;
    let c_path = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    // SAFETY: `c_path` is a valid NUL-terminated string; `buf` is zeroed and
    // fully written by a successful call.
    let mut buf: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut buf) } != 0 {
        return None;
    }
    Some((buf.f_bavail as u64).saturating_mul(buf.f_frsize as u64))
}

/// Device id of the volume holding `path` (nearest existing ancestor, symlinks
/// followed), for telling two paths on one filesystem apart from two volumes.
pub fn device_id(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(nearest_existing(path)?).ok().map(|m| m.dev())
}

fn nearest_existing(path: &Path) -> Option<&Path> {
    let mut candidate = Some(path);
    while let Some(dir) = candidate {
        if dir.exists() {
            return Some(dir);
        }
        candidate = dir.parent();
    }
    None
}

// ---------------------------------------------------------------------------
// Selection (pure).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A session dir under `incremental/`: pure rebuild cache.
    Incremental,
    /// A file or dir under `deps/`, `build/` or `.fingerprint/`.
    Artifact,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub path: PathBuf,
    pub bytes: u64,
    /// Days since a compile last needed the owning unit (see the module docs).
    pub idle_days: f64,
    pub kind: Kind,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Policy {
    pub incremental_days: f64,
    pub artifact_days: f64,
}

impl Default for Policy {
    fn default() -> Self {
        Self { incremental_days: 2.0, artifact_days: 7.0 }
    }
}

impl Policy {
    pub fn from_env() -> Self {
        let d = Self::default();
        Self {
            incremental_days: env_f64("AUGMENTAGENT_PRUNE_INCREMENTAL_DAYS", d.incremental_days),
            artifact_days: env_f64("AUGMENTAGENT_PRUNE_ARTIFACT_DAYS", d.artifact_days),
        }
    }
}

/// Age-based selection: everything idle past its kind's threshold.
pub fn select_idle(cands: &[Candidate], policy: &Policy) -> Vec<Candidate> {
    cands
        .iter()
        .filter(|c| match c.kind {
            Kind::Incremental => c.idle_days >= policy.incremental_days,
            Kind::Artifact => c.idle_days >= policy.artifact_days,
        })
        .cloned()
        .collect()
}

/// What a cap-driven trim removes, cheapest loss first (#1407).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CapPlan {
    pub remove: Vec<Candidate>,
    /// Tiers 1–2 could not get under the cap: the caller drops the whole
    /// profile dir (one cold rebuild), exactly what #891 always did.
    pub full_drop: bool,
}

/// Bring a `total_bytes` dir under `cap_bytes`:
/// 1. all `incremental/` (pure rebuild cache);
/// 2. artifacts idle at least `stale_days`, longest-idle first, stopping as
///    soon as the dir fits;
/// 3. otherwise ask for the full drop.
pub fn plan_cap_trim(
    cands: &[Candidate],
    total_bytes: u64,
    cap_bytes: u64,
    stale_days: f64,
) -> CapPlan {
    let mut plan = CapPlan::default();
    if total_bytes <= cap_bytes {
        return plan;
    }
    let mut left = total_bytes;
    for c in cands.iter().filter(|c| c.kind == Kind::Incremental) {
        left = left.saturating_sub(c.bytes);
        plan.remove.push(c.clone());
    }
    if left <= cap_bytes {
        return plan;
    }
    let mut stale: Vec<&Candidate> = cands
        .iter()
        .filter(|c| c.kind == Kind::Artifact && c.idle_days >= stale_days)
        .collect();
    stale.sort_by(|a, b| b.idle_days.total_cmp(&a.idle_days));
    for c in stale {
        if left <= cap_bytes {
            break;
        }
        left = left.saturating_sub(c.bytes);
        plan.remove.push(c.clone());
    }
    plan.full_drop = left > cap_bytes;
    plan
}

/// The 16-hex unit hash cargo appends to an artifact name:
/// `libserde-0123456789abcdef.rlib`, `proc-macro2-0123456789abcdef`.
pub fn unit_hash(file_name: &str) -> Option<&str> {
    let stem = file_name.split('.').next().unwrap_or(file_name);
    let hash = stem.rsplit('-').next()?;
    (hash.len() == 16 && stem.len() > 17 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
        .then_some(hash)
}

/// One `.fingerprint/<pkg>-<hash>/` dir as cargo wrote it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct UnitRecord {
    /// The dir-name hash, shared with the unit's files in `deps/`, `build/`.
    pub dir_hash: String,
    /// Fingerprint hashes this dir currently holds (one per target kind).
    pub fingerprints: Vec<String>,
    /// Fingerprint hashes of the units it was last compiled against.
    pub deps: Vec<String>,
    /// Days since it was last compiled.
    pub compiled_days: f64,
}

/// Days since a compile last needed each unit, keyed by dir hash: its own
/// compile, or the newest compile of anything that (transitively) depends on
/// it. `None` when the records do not resolve — fewer than half of the
/// recorded dependency edges name a known unit — which means the format
/// moved and nothing here may be trusted.
pub fn last_needed_days(units: &[UnitRecord]) -> Option<HashMap<String, f64>> {
    let by_fp: HashMap<&str, usize> = units
        .iter()
        .enumerate()
        .flat_map(|(i, u)| u.fingerprints.iter().map(move |f| (f.as_str(), i)))
        .collect();
    let (mut edges, mut resolved) = (0usize, 0usize);
    let children: Vec<Vec<usize>> = units
        .iter()
        .map(|u| {
            edges += u.deps.len();
            let kids: Vec<usize> =
                u.deps.iter().filter_map(|d| by_fp.get(d.as_str()).copied()).collect();
            resolved += kids.len();
            kids
        })
        .collect();
    if edges > 0 && resolved * 2 < edges {
        return None;
    }
    // Newest compile first: the first visit to a unit is by its most recently
    // compiled ancestor, so each unit is settled exactly once.
    let mut order: Vec<usize> = (0..units.len()).collect();
    order.sort_by(|a, b| units[*a].compiled_days.total_cmp(&units[*b].compiled_days));
    let mut needed: Vec<Option<f64>> = vec![None; units.len()];
    for start in order {
        if needed[start].is_some() {
            continue;
        }
        let days = units[start].compiled_days;
        let mut stack = vec![start];
        while let Some(i) = stack.pop() {
            if needed[i].is_some() {
                continue;
            }
            needed[i] = Some(days);
            stack.extend(children[i].iter().copied());
        }
    }
    // Two fingerprint dirs never share a hash, but be exact if they do.
    let mut out: HashMap<String, f64> = HashMap::new();
    for (u, days) in units.iter().zip(needed) {
        let days = days.unwrap_or(u.compiled_days);
        out.entry(u.dir_hash.clone()).and_modify(|d| *d = d.min(days)).or_insert(days);
    }
    Some(out)
}

/// Cargo stores a dependency's fingerprint as a `u64` and the unit's own as
/// the hex of its little-endian bytes.
pub fn fingerprint_hex(value: u64) -> String {
    value.to_le_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

/// Read one fingerprint dir. `None` when its name carries no unit hash.
fn read_unit(dir: &Path, now: SystemTime) -> Option<UnitRecord> {
    let dir_hash = unit_hash(dir.file_name()?.to_str()?)?.to_string();
    let mut unit = UnitRecord { dir_hash, compiled_days: f64::MAX, ..Default::default() };
    for file in read_dir_paths(dir) {
        let name = file.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
        if let Some(t) = last_touch(&file) {
            unit.compiled_days = unit.compiled_days.min(idle_days(now, t));
        }
        if name.ends_with(".json") {
            let deps = std::fs::read_to_string(&file)
                .ok()
                .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                .and_then(|v| v.get("deps")?.as_array().cloned())
                .unwrap_or_default();
            unit.deps.extend(
                deps.iter().filter_map(|d| d.get(3)?.as_u64()).map(fingerprint_hex),
            );
        } else if !name.starts_with("dep-") && name != "invoked.timestamp" && !name.contains("output") {
            // `<kind>-<target>`: the unit's current fingerprint, 16 hex chars.
            if let Ok(text) = std::fs::read_to_string(&file) {
                let text = text.trim();
                if text.len() == 16 && text.bytes().all(|b| b.is_ascii_hexdigit()) {
                    unit.fingerprints.push(text.to_string());
                }
            }
        }
    }
    Some(unit)
}

// ---------------------------------------------------------------------------
// Scan (filesystem).
// ---------------------------------------------------------------------------

/// Profile dirs (`debug`, `release`, `<triple>/debug`, …) under a target
/// root: any dir holding `deps/` or `incremental/`.
pub fn profile_dirs(target_root: &Path) -> Vec<PathBuf> {
    let is_profile = |p: &Path| p.join("deps").is_dir() || p.join("incremental").is_dir();
    let mut out = Vec::new();
    for child in read_dir_paths(target_root) {
        if !child.is_dir() {
            continue;
        }
        if is_profile(&child) {
            out.push(child);
        } else {
            out.extend(read_dir_paths(&child).into_iter().filter(|p| p.is_dir() && is_profile(p)));
        }
    }
    out.sort();
    out
}

fn read_dir_paths(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|rd| rd.flatten().map(|e| e.path()).collect())
        .unwrap_or_default()
}

/// Bytes actually allocated under `path` (not following symlinks).
pub fn disk_bytes(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    let own = meta.blocks().saturating_mul(512);
    if !meta.file_type().is_dir() {
        return own;
    }
    own + read_dir_paths(path).iter().map(|p| disk_bytes(p)).sum::<u64>()
}

fn idle_days(now: SystemTime, last: SystemTime) -> f64 {
    now.duration_since(last).unwrap_or(Duration::ZERO).as_secs_f64() / DAY
}

fn last_touch(path: &Path) -> Option<SystemTime> {
    std::fs::symlink_metadata(path).ok()?.modified().ok()
}

/// Latest write across a dir's direct children (and the dir itself).
fn last_touch_shallow(dir: &Path) -> Option<SystemTime> {
    read_dir_paths(dir).iter().filter_map(|p| last_touch(p)).chain(last_touch(dir)).max()
}

/// Every prunable entry of one profile dir. Top-level binaries (the deployed
/// daemon lives there) are never listed. The flag is `false` when the
/// fingerprint records did not resolve and artifacts were left out.
pub fn scan_profile(profile: &Path, now: SystemTime) -> (Vec<Candidate>, bool) {
    let mut out = Vec::new();
    for session in read_dir_paths(&profile.join("incremental")) {
        // Cargo rewrites a session dir on every compile of its crate.
        if let Some(t) = last_touch_shallow(&session) {
            out.push(Candidate {
                bytes: disk_bytes(&session),
                idle_days: idle_days(now, t),
                kind: Kind::Incremental,
                path: session,
            });
        }
    }

    let fingerprints = read_dir_paths(&profile.join(".fingerprint"));
    let units: Vec<UnitRecord> = fingerprints.iter().filter_map(|d| read_unit(d, now)).collect();
    let Some(needed) = last_needed_days(&units) else {
        return (out, false);
    };

    let artifacts = fingerprints
        .into_iter()
        .chain(read_dir_paths(&profile.join("deps")))
        .chain(read_dir_paths(&profile.join("build")));
    for path in artifacts {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        // A unit's record speaks for all its files; an artifact with no
        // record left is already orphaned and judged on its own age.
        let idle = match unit_hash(name).and_then(|h| needed.get(h)) {
            Some(days) => *days,
            None => {
                let own = if path.is_dir() { last_touch_shallow(&path) } else { last_touch(&path) };
                match own {
                    Some(t) => idle_days(now, t),
                    None => continue,
                }
            }
        };
        out.push(Candidate { bytes: disk_bytes(&path), idle_days: idle, kind: Kind::Artifact, path });
    }
    (out, true)
}

// ---------------------------------------------------------------------------
// In-use probe.
// ---------------------------------------------------------------------------

/// What a running cargo/rustc says about where it builds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuildProc {
    pub cwd: Option<PathBuf>,
    /// `CARGO_TARGET_DIR` from its environment, when set.
    pub target_env: Option<PathBuf>,
    pub cmdline: String,
}

/// Running cargo/rustc processes. `None` = cannot tell (no `/proc`), which
/// callers must treat as "everything is in use".
pub fn build_procs() -> Option<Vec<BuildProc>> {
    let rd = std::fs::read_dir("/proc").ok()?;
    let mut out = Vec::new();
    for entry in rd.flatten() {
        let pid = entry.file_name();
        if !pid.to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let dir = entry.path();
        let comm = std::fs::read_to_string(dir.join("comm")).unwrap_or_default();
        if !matches!(comm.trim(), "cargo" | "rustc" | "rustdoc" | "cargo-clippy" | "clippy-driver") {
            continue;
        }
        let nul_sep = |name: &str| -> Vec<String> {
            std::fs::read(dir.join(name))
                .map(|b| {
                    b.split(|c| *c == 0)
                        .map(|s| String::from_utf8_lossy(s).into_owned())
                        .collect()
                })
                .unwrap_or_default()
        };
        out.push(BuildProc {
            cwd: std::fs::read_link(dir.join("cwd")).ok(),
            target_env: nul_sep("environ")
                .iter()
                .find_map(|kv| kv.strip_prefix("CARGO_TARGET_DIR=").map(PathBuf::from)),
            cmdline: nul_sep("cmdline").join(" "),
        });
    }
    Some(out)
}

/// Is `root` (already canonical) the target dir of any running build?
/// `resolve` canonicalizes a path (injected so tests need no filesystem).
pub fn root_in_use(
    root: &Path,
    procs: &[BuildProc],
    resolve: &dyn Fn(&Path) -> Option<PathBuf>,
) -> bool {
    let root_str = root.to_string_lossy();
    procs.iter().any(|p| {
        if let Some(t) = &p.target_env {
            // An explicit target dir is the whole answer for this process.
            return resolve(t).as_deref() == Some(root) || t == root;
        }
        if p.cmdline.contains(root_str.as_ref()) {
            return true;
        }
        // Default layout: `<workspace>/target`, the workspace being the cwd
        // or one of its ancestors.
        p.cwd.as_deref().is_some_and(|cwd| {
            cwd.ancestors()
                .any(|a| resolve(&a.join("target")).as_deref() == Some(root))
        })
    })
}

/// `true` when `lock` exists and another process holds its `flock` — a lane
/// mid-run. A missing file is not held.
pub fn lock_is_held(lock: &Path) -> bool {
    use std::os::unix::io::AsRawFd;
    let Ok(file) = std::fs::File::open(lock) else {
        return false;
    };
    // SAFETY: `file` owns the fd for the whole call; `flock` neither reads
    // nor writes through it.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        return false;
    }
    true
}

// ---------------------------------------------------------------------------
// Roots.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root {
    pub label: String,
    /// Canonical path of the target dir.
    pub path: PathBuf,
    /// The shared gate cache: also skipped while a lane lock is held.
    pub gate: bool,
    /// A worktree whose branch is merged or gone: its target goes whole.
    pub orphan: bool,
    /// Canonical profile dirs to prune, symlinks resolved.
    pub profiles: Vec<PathBuf>,
}

impl Root {
    /// A plain root at `path` (canonical), with its profile dirs resolved.
    pub fn at(label: &str, path: &Path) -> Self {
        let mut profiles: Vec<PathBuf> = profile_dirs(path)
            .iter()
            .filter_map(|p| std::fs::canonicalize(p).ok())
            .collect();
        profiles.dedup();
        Self { label: label.into(), path: path.to_path_buf(), gate: false, orphan: false, profiles }
    }

    /// Bytes this root holds, counting profile dirs that live elsewhere.
    fn bytes(&self) -> u64 {
        disk_bytes(&self.path)
            + self
                .profiles
                .iter()
                .filter(|p| !p.starts_with(&self.path))
                .map(|p| disk_bytes(p))
                .sum::<u64>()
    }
}

pub fn gate_target_dir() -> PathBuf {
    std::env::var("AUGMENTAGENT_GATE_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            PathBuf::from(home).join(".cache/augmentagent-gate-target")
        })
}

/// Extra target dirs to prune, colon-separated, from the local `.env`.
pub fn extra_roots_from(value: &str) -> Vec<PathBuf> {
    value
        .split(':')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// One worktree from `git worktree list --porcelain`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Worktree {
    pub path: PathBuf,
    pub head: String,
    pub branch: Option<String>,
}

pub fn parse_worktrees(porcelain: &str) -> Vec<Worktree> {
    let mut out = Vec::new();
    let mut cur: Option<Worktree> = None;
    for line in porcelain.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            out.extend(cur.take());
            cur = Some(Worktree { path: PathBuf::from(p), ..Default::default() });
        } else if let (Some(h), Some(w)) = (line.strip_prefix("HEAD "), cur.as_mut()) {
            w.head = h.to_string();
        } else if let (Some(b), Some(w)) = (line.strip_prefix("branch "), cur.as_mut()) {
            w.branch = Some(b.trim_start_matches("refs/heads/").to_string());
        }
    }
    out.extend(cur);
    out
}

fn git(repo: &Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git").arg("-C").arg(repo).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A worktree's work is on `origin/main` (fast-forward or merge commit), or
/// its branch's upstream was deleted (how a squash merge looks locally).
fn worktree_is_merged(repo: &Path, wt: &Worktree) -> bool {
    let ancestor = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["merge-base", "--is-ancestor", &wt.head, "origin/main"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ancestor {
        return true;
    }
    wt.branch.as_deref().is_some_and(|b| {
        git(repo, &["for-each-ref", "--format=%(upstream:track)", &format!("refs/heads/{b}")])
            .is_some_and(|t| t.contains("gone"))
    })
}

/// Every target dir this repo owns: the checkout's own, each worktree's, the
/// gate cache and the opt-in extras. De-duplicated by canonical path, so a
/// `target/debug` symlinked into the gate cache is pruned once.
pub fn discover_roots(repo_root: &Path, extra: &[PathBuf]) -> Vec<Root> {
    let mut roots: Vec<Root> = Vec::new();
    let mut seen_profiles: Vec<PathBuf> = Vec::new();
    let mut add = |label: String, path: &Path, gate: bool, orphan: bool| {
        let Ok(canon) = std::fs::canonicalize(path) else { return };
        if roots.iter().any(|r| r.path == canon) {
            return;
        }
        // A profile dir may be a symlink onto another volume
        // (`target/debug -> /big/disk/x`). It is pruned through the link,
        // and only once however many roots point at it. Its parent over
        // there is NOT ours and is never treated as a root.
        let mut root = Root::at(&label, &canon);
        root.gate = gate;
        root.orphan = orphan;
        root.profiles.retain(|p| !seen_profiles.contains(p));
        seen_profiles.extend(root.profiles.iter().cloned());
        roots.push(root);
    };

    add("gate cache".into(), &gate_target_dir(), true, false);
    let worktrees = git(repo_root, &["worktree", "list", "--porcelain"])
        .map(|s| parse_worktrees(&s))
        .unwrap_or_default();
    let primary = worktrees.first().map(|w| w.path.clone());
    if worktrees.is_empty() {
        add("checkout".into(), &repo_root.join("target"), false, false);
    }
    for wt in &worktrees {
        let is_primary = Some(&wt.path) == primary.as_ref();
        let label = if is_primary {
            "checkout".to_string()
        } else {
            let name = wt.path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
            format!("worktree {name}")
        };
        let orphan = !is_primary && worktree_is_merged(repo_root, wt);
        add(label, &wt.path.join("target"), false, orphan);
    }
    for (i, dir) in extra.iter().enumerate() {
        add(format!("extra {}", i + 1), dir, false, false);
    }
    roots
}

// ---------------------------------------------------------------------------
// Filesystem watch (#1409).
// ---------------------------------------------------------------------------

/// Free space on one filesystem, and everything of ours that lives on it.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct FsReading {
    pub labels: Vec<String>,
    pub dev: u64,
    pub free_gb: f64,
}

impl FsReading {
    /// "the debug target and the gate target".
    pub fn label(&self) -> String {
        self.labels.join(" and ")
    }
}

/// Every place this project writes in bulk. Build output is routinely moved
/// with a symlink or `CARGO_TARGET_DIR`, so the profile dirs are named
/// individually and resolved: the volume that fills is the one the link
/// points at, not the one the checkout sits on.
pub fn watched_paths(repo_root: &Path) -> Vec<(String, PathBuf)> {
    let resolved = |p: PathBuf| std::fs::canonicalize(&p).unwrap_or(p);
    let mut out = vec![("/".to_string(), PathBuf::from("/"))];
    if let Some(dir) = augmentagent_channel_core::state_dir::state_dir() {
        out.push(("the state dir".into(), dir));
    }
    out.push(("the temp dir".into(), std::env::temp_dir()));
    out.push(("the gate target".into(), resolved(gate_target_dir())));
    out.push(("the debug target".into(), resolved(repo_root.join("target/debug"))));
    out.push(("the release target".into(), resolved(repo_root.join("target/release"))));
    let extra = extra_roots_from(&std::env::var("AUGMENTAGENT_PRUNE_TARGET_DIRS").unwrap_or_default());
    for (i, dir) in extra.into_iter().enumerate() {
        out.push((format!("extra target {}", i + 1), resolved(dir)));
    }
    out
}

/// One reading per filesystem, in first-seen order. Pure: `(label, device,
/// free GB)` in, so two paths on one mount are reported once.
pub fn merge_by_device(probes: &[(String, u64, f64)]) -> Vec<FsReading> {
    let mut out: Vec<FsReading> = Vec::new();
    for (label, dev, free_gb) in probes {
        match out.iter_mut().find(|r| r.dev == *dev) {
            Some(r) => r.labels.push(label.clone()),
            None => out.push(FsReading { labels: vec![label.clone()], dev: *dev, free_gb: *free_gb }),
        }
    }
    out
}

pub fn read_filesystems(repo_root: &Path) -> Vec<FsReading> {
    let probes: Vec<(String, u64, f64)> = watched_paths(repo_root)
        .into_iter()
        .filter_map(|(label, path)| {
            Some((label, device_id(&path)?, available_bytes(&path)? as f64 / GIB))
        })
        .collect();
    merge_by_device(&probes)
}

/// A filesystem's free space at an earlier check.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PastReading {
    pub free_gb: f64,
    pub at: chrono::DateTime<chrono::Utc>,
}

/// A filesystem losing space fast: `lost_gb` over `hours`.
#[derive(Debug, Clone, PartialEq)]
pub struct FsDrop {
    pub label: String,
    pub lost_gb: f64,
    pub hours: f64,
    pub free_gb: f64,
}

/// Filesystems that lost at least `min_drop_gb` since their previous reading.
/// A reading older than `max_age_hours` is no baseline (the watchdog was off;
/// a week of ordinary growth is not a runaway writer).
pub fn fs_drops(
    prev: &HashMap<String, PastReading>,
    now: &[FsReading],
    at: chrono::DateTime<chrono::Utc>,
    min_drop_gb: f64,
    max_age_hours: f64,
) -> Vec<FsDrop> {
    now.iter()
        .filter_map(|r| {
            let past = prev.get(&r.dev.to_string())?;
            let hours = (at - past.at).num_seconds() as f64 / 3600.0;
            let lost_gb = past.free_gb - r.free_gb;
            (hours > 0.0 && hours <= max_age_hours && lost_gb >= min_drop_gb).then(|| FsDrop {
                label: r.label(),
                lost_gb,
                hours,
                free_gb: r.free_gb,
            })
        })
        .collect()
}

fn readings_path() -> Option<PathBuf> {
    augmentagent_channel_core::state_dir::state_dir().map(|d| d.join("disk-readings.json"))
}

pub fn load_readings() -> HashMap<String, PastReading> {
    readings_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Remember this check's readings for the next one. Best effort.
pub fn save_readings(now: &[FsReading], at: chrono::DateTime<chrono::Utc>) {
    let Some(path) = readings_path() else { return };
    let map: HashMap<String, PastReading> = now
        .iter()
        .map(|r| (r.dev.to_string(), PastReading { free_gb: r.free_gb, at }))
        .collect();
    if let Ok(text) = serde_json::to_string(&map) {
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

/// `augmentagent disk status`: what is watched and how much room it has.
pub fn run_status(repo_root: &Path, json: bool, out: &mut dyn Write) -> Result<()> {
    let readings = read_filesystems(repo_root);
    if json {
        writeln!(out, "{}", serde_json::to_string_pretty(&readings)?)?;
        return Ok(());
    }
    let floor = gate_min_free_gb();
    for r in &readings {
        let flag = if r.free_gb < floor { "  LOW" } else { "" };
        writeln!(out, "{:>8.1} GB free{flag}  {}", r.free_gb, r.label())?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Run.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub dry_run: bool,
    pub json: bool,
    /// Prune exactly these dirs instead of discovering the repo's.
    pub roots: Vec<PathBuf>,
    /// With `roots`: trim to this size in tiers (#1407) instead of by age.
    pub cap_mb: Option<u64>,
    /// Idle days an artifact needs before a cap trim may take it.
    pub stale_days: Option<f64>,
}

#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct RootReport {
    pub label: String,
    pub path: String,
    pub outcome: String,
    pub removed: usize,
    pub freed_bytes: u64,
    pub full_drop: bool,
}

/// An orphaned worktree's target is removed whole once nothing in it has been
/// written for this long — a worktree parked on `main` an hour ago is not one.
const ORPHAN_QUIET_DAYS: f64 = 1.0;

fn remove(path: &Path) -> bool {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return false,
    };
    if meta.file_type().is_dir() {
        std::fs::remove_dir_all(path).is_ok()
    } else {
        std::fs::remove_file(path).is_ok()
    }
}

fn newest_write(profiles: &[PathBuf]) -> Option<SystemTime> {
    profiles
        .iter()
        .flat_map(|p| [p.join("deps"), p.join(".fingerprint"), p.join("incremental")])
        .filter_map(|d| last_touch(&d))
        .max()
}

/// Prune one root. `busy` is the in-use verdict, decided by the caller.
pub fn prune_root(
    root: &Root,
    busy: Option<&str>,
    opts: &Options,
    policy: &Policy,
    now: SystemTime,
) -> RootReport {
    let mut report = RootReport {
        label: root.label.clone(),
        path: root.path.display().to_string(),
        outcome: String::new(),
        removed: 0,
        freed_bytes: 0,
        full_drop: false,
    };
    if let Some(why) = busy {
        report.outcome = format!("skipped: {why}");
        return report;
    }
    let verb = if opts.dry_run { "would remove" } else { "removed" };

    if root.orphan {
        let quiet = newest_write(&root.profiles).map_or(f64::MAX, |t| idle_days(now, t));
        if quiet >= ORPHAN_QUIET_DAYS {
            report.freed_bytes = root.bytes();
            report.removed = 1;
            report.full_drop = true;
            if !opts.dry_run {
                for p in root.profiles.iter().filter(|p| !p.starts_with(&root.path)) {
                    remove(p);
                }
                remove(&root.path);
            }
            report.outcome = format!("{verb} the whole target (branch merged or gone)");
            return report;
        }
    }

    let profiles = &root.profiles;
    let mut cands: Vec<Candidate> = Vec::new();
    let mut understood = true;
    for p in profiles {
        let (found, ok) = scan_profile(p, now);
        cands.extend(found);
        understood &= ok;
    }

    let picked = match opts.cap_mb {
        Some(cap_mb) => {
            let total = root.bytes();
            let stale = opts.stale_days.unwrap_or(3.0);
            let plan = plan_cap_trim(&cands, total, cap_mb.saturating_mul(1024 * 1024), stale);
            report.full_drop = plan.full_drop;
            plan.remove
        }
        None => select_idle(&cands, policy),
    };
    for c in &picked {
        if opts.dry_run || remove(&c.path) {
            report.removed += 1;
            report.freed_bytes += c.bytes;
        }
    }
    if report.full_drop {
        // Tiers 1–2 were not enough: drop every profile dir, keeping the
        // root itself (and the gate's own `tmp/`) in place.
        for p in profiles {
            report.freed_bytes += disk_bytes(p);
            if !opts.dry_run {
                remove(p);
            }
        }
        report.outcome = format!("{verb} every profile dir (still over the cap after tiers 1-2)");
    } else {
        let note = if understood {
            ""
        } else {
            " (fingerprint records not understood: artifacts kept, incremental only)"
        };
        report.outcome = format!("{verb} {} cold entries{note}", report.removed);
    }
    report
}

fn lane_locks() -> Vec<PathBuf> {
    let base = std::env::var("AUGMENTAGENT_SELFIMPROVE_LOCK")
        .ok()
        .filter(|p| !p.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            augmentagent_channel_core::state_dir::state_dir().map(|d| d.join("self-improve.lock"))
        });
    let Some(base) = base else { return Vec::new() };
    let resume = PathBuf::from(format!(
        "{}-resume.lock",
        base.to_string_lossy().trim_end_matches(".lock")
    ));
    vec![base, resume]
}

fn fmt_gb(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / GIB)
}

pub fn run_prune(repo_root: &Path, opts: &Options, out: &mut dyn Write) -> Result<()> {
    let roots: Vec<Root> = if opts.roots.is_empty() {
        let extra = extra_roots_from(
            &std::env::var("AUGMENTAGENT_PRUNE_TARGET_DIRS").unwrap_or_default(),
        );
        discover_roots(repo_root, &extra)
    } else {
        let gate = std::fs::canonicalize(gate_target_dir()).ok();
        opts.roots
            .iter()
            .filter_map(|p| std::fs::canonicalize(p).ok())
            .map(|path| {
                let mut root = Root::at("root", &path);
                root.gate = Some(&path) == gate.as_ref();
                root
            })
            .collect()
    };
    let procs = build_procs();
    let lane_busy = lane_locks().iter().any(|l| lock_is_held(l));
    let resolve = |p: &Path| std::fs::canonicalize(p).ok();
    let policy = Policy::from_env();
    let now = SystemTime::now();

    let mut reports = Vec::new();
    for root in &roots {
        let busy = if root.gate && lane_busy {
            Some("an auto-PR lane is building")
        } else {
            match &procs {
                None => Some("cannot list running builds on this platform"),
                Some(procs) if root_in_use(&root.path, procs, &resolve) => {
                    Some("a cargo build is using it")
                }
                Some(_) => None,
            }
        };
        reports.push(prune_root(root, busy, opts, &policy, now));
    }

    if opts.json {
        writeln!(out, "{}", serde_json::to_string_pretty(&reports)?)?;
    } else {
        for r in &reports {
            writeln!(
                out,
                "{:<22} {:>9}  {}  [{}]",
                r.label,
                fmt_gb(r.freed_bytes),
                r.outcome,
                r.path
            )?;
        }
        let total: u64 = reports.iter().map(|r| r.freed_bytes).sum();
        let verb = if opts.dry_run { "would free" } else { "freed" };
        writeln!(out, "{verb} {} across {} target dirs", fmt_gb(total), reports.len())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(path: &str, mb: u64, idle: f64, kind: Kind) -> Candidate {
        Candidate { path: PathBuf::from(path), bytes: mb * 1024 * 1024, idle_days: idle, kind }
    }
    const MB: u64 = 1024 * 1024;

    #[test]
    fn floor_refusal_is_pure_over_a_byte_count() {
        let gib = 1024 * 1024 * 1024u64;
        assert!(floor_refusal(20 * gib, 15.0).is_none());
        assert!(floor_refusal(15 * gib, 15.0).is_none(), "the floor itself is enough");
        let why = floor_refusal(4 * gib, 15.0).expect("under the floor refuses");
        assert!(why.starts_with(LOW_DISK_REFUSAL), "{why}");
        assert!(why.contains("4.0 GB free < 15 GB"), "{why}");
        assert!(floor_refusal(0, 0.0).is_none(), "a zero floor disables the guard");
    }

    #[test]
    fn unit_hash_reads_cargos_artifact_names() {
        assert_eq!(unit_hash("libserde-0123456789abcdef.rlib"), Some("0123456789abcdef"));
        assert_eq!(unit_hash("proc-macro2-0123456789abcdef"), Some("0123456789abcdef"));
        assert_eq!(unit_hash("serde-0123456789abcdef.d"), Some("0123456789abcdef"));
        assert_eq!(unit_hash("augmentagent"), None, "a top-level binary has no hash");
        assert_eq!(unit_hash("0123456789abcdef"), None, "a bare hash names no unit");
        assert_eq!(unit_hash("lib-0123456789abcdeg.rlib"), None, "not hex");
        assert_eq!(unit_hash("s-hlzw8kmfkj-0b4zshu-working"), None);
    }

    #[test]
    fn idle_selection_uses_each_kinds_threshold() {
        let cands = [
            cand("inc/old", 1, 3.0, Kind::Incremental),
            cand("inc/new", 1, 1.0, Kind::Incremental),
            cand("deps/old", 1, 8.0, Kind::Artifact),
            cand("deps/warm", 1, 3.0, Kind::Artifact),
        ];
        let picked = select_idle(&cands, &Policy::default());
        let names: Vec<_> = picked.iter().map(|c| c.path.to_str().unwrap()).collect();
        assert_eq!(names, ["inc/old", "deps/old"]);
    }

    fn unit(dir: &str, fp: &str, deps: &[&str], compiled: f64) -> UnitRecord {
        UnitRecord {
            dir_hash: dir.into(),
            fingerprints: vec![fp.into()],
            deps: deps.iter().map(|d| d.to_string()).collect(),
            compiled_days: compiled,
        }
    }

    #[test]
    fn a_unit_is_needed_as_of_the_newest_compile_that_depends_on_it() {
        // app (today) -> mid (20d) -> leaf (40d); old_app (30d) -> old_leaf (40d).
        let units = [
            unit("app", "f-app", &["f-mid"], 0.0),
            unit("mid", "f-mid", &["f-leaf"], 20.0),
            unit("leaf", "f-leaf", &[], 40.0),
            unit("old_app", "f-old", &["f-oldleaf"], 30.0),
            unit("old_leaf", "f-oldleaf", &[], 40.0),
        ];
        let needed = last_needed_days(&units).expect("records resolve");
        assert_eq!(needed["app"], 0.0);
        assert_eq!(needed["mid"], 0.0, "linked by a build compiled today");
        assert_eq!(needed["leaf"], 0.0, "transitively");
        assert_eq!(needed["old_app"], 30.0);
        assert_eq!(needed["old_leaf"], 30.0, "only as fresh as its newest dependent");
    }

    #[test]
    fn a_shared_dependency_takes_its_newest_dependent_and_cycles_terminate() {
        let units = [
            unit("old", "f-old", &["f-shared"], 30.0),
            unit("new", "f-new", &["f-shared"], 1.0),
            unit("shared", "f-shared", &["f-new"], 50.0),
        ];
        let needed = last_needed_days(&units).unwrap();
        assert_eq!(needed["shared"], 1.0);
    }

    #[test]
    fn records_that_do_not_resolve_are_not_trusted() {
        // A cargo that writes dependencies some other way: every edge dangles.
        let units = [
            unit("app", "f-app", &["?1", "?2"], 0.0),
            unit("leaf", "f-leaf", &["?3"], 40.0),
        ];
        assert_eq!(last_needed_days(&units), None);
        // A few stale edges (a dependency recompiled since) are normal.
        let units = [
            unit("app", "f-app", &["f-leaf", "f-leaf", "stale"], 0.0),
            unit("leaf", "f-leaf", &[], 40.0),
        ];
        assert!(last_needed_days(&units).is_some());
        assert!(last_needed_days(&[]).is_some(), "an empty dir is trivially understood");
    }

    #[test]
    fn fingerprint_hex_matches_cargos_on_disk_form() {
        // Observed pair: `"deps": [[…, "rusqlite", false, 4798693716673416483]]`
        // and that unit's `lib-rusqlite` file.
        assert_eq!(fingerprint_hex(4798693716673416483), "23cd37c082629842");
    }

    #[test]
    fn cap_trim_does_nothing_under_the_cap() {
        let cands = [cand("inc/a", 500, 9.0, Kind::Incremental)];
        assert_eq!(plan_cap_trim(&cands, 900 * MB, 1000 * MB, 3.0), CapPlan::default());
    }

    #[test]
    fn cap_trim_stops_at_incremental_when_that_is_enough() {
        let cands = [
            cand("inc/a", 300, 0.1, Kind::Incremental),
            cand("deps/stale", 400, 9.0, Kind::Artifact),
        ];
        let plan = plan_cap_trim(&cands, 1200 * MB, 1000 * MB, 3.0);
        assert_eq!(plan.remove.len(), 1, "tier 1 alone fits: {plan:?}");
        assert_eq!(plan.remove[0].kind, Kind::Incremental);
        assert!(!plan.full_drop);
    }

    #[test]
    fn cap_trim_takes_stale_artifacts_longest_idle_first_and_stops_when_it_fits() {
        let cands = [
            cand("inc/a", 100, 0.1, Kind::Incremental),
            cand("deps/five", 300, 5.0, Kind::Artifact),
            cand("deps/nine", 300, 9.0, Kind::Artifact),
            cand("deps/warm", 900, 0.5, Kind::Artifact),
        ];
        // 1600 → 1500 after tier 1; needs 300 more → the 9-day one only.
        let plan = plan_cap_trim(&cands, 1600 * MB, 1200 * MB, 3.0);
        let names: Vec<_> = plan.remove.iter().map(|c| c.path.to_str().unwrap()).collect();
        assert_eq!(names, ["inc/a", "deps/nine"]);
        assert!(!plan.full_drop);
    }

    #[test]
    fn cap_trim_never_takes_a_warm_artifact_and_asks_for_the_full_drop_instead() {
        let cands = [
            cand("inc/a", 100, 0.1, Kind::Incremental),
            cand("deps/warm", 1900, 0.5, Kind::Artifact),
        ];
        let plan = plan_cap_trim(&cands, 2000 * MB, 1000 * MB, 3.0);
        assert!(plan.full_drop);
        assert!(plan.remove.iter().all(|c| c.path != PathBuf::from("deps/warm")));
    }

    #[test]
    fn a_root_is_in_use_by_explicit_target_dir_cmdline_or_default_layout() {
        let resolve = |p: &Path| -> Option<PathBuf> {
            match p.to_str()? {
                "/repo/target" => Some("/big/repo-target".into()),
                "/link/gate" => Some("/big/gate".into()),
                other => Some(other.into()),
            }
        };
        let via_env = BuildProc { target_env: Some("/link/gate".into()), ..Default::default() };
        assert!(root_in_use(Path::new("/big/gate"), &[via_env.clone()], &resolve));
        assert!(
            !root_in_use(Path::new("/big/repo-target"), &[via_env], &resolve),
            "an explicit target dir means the cwd's ./target is NOT in use"
        );
        let via_cmd = BuildProc {
            cmdline: "rustc --out-dir /big/gate/debug/deps".into(),
            ..Default::default()
        };
        assert!(root_in_use(Path::new("/big/gate"), &[via_cmd], &resolve));
        let via_cwd = BuildProc { cwd: Some("/repo/crates/x".into()), ..Default::default() };
        assert!(root_in_use(Path::new("/big/repo-target"), &[via_cwd], &resolve));
        assert!(!root_in_use(Path::new("/big/gate"), &[], &resolve));
    }

    #[test]
    fn worktree_porcelain_parses_paths_heads_and_branches() {
        let text = "worktree /repo\nHEAD aaa\nbranch refs/heads/main\n\n\
                    worktree /repo/.wt/x\nHEAD bbb\nbranch refs/heads/feat/x\n\n\
                    worktree /repo/.wt/detached\nHEAD ccc\ndetached\n";
        let wts = parse_worktrees(text);
        assert_eq!(wts.len(), 3);
        assert_eq!(wts[1].path, PathBuf::from("/repo/.wt/x"));
        assert_eq!(wts[1].branch.as_deref(), Some("feat/x"));
        assert_eq!(wts[2].branch, None);
        assert_eq!(wts[2].head, "ccc");
    }

    #[test]
    fn extra_roots_split_on_colons_and_ignore_blanks() {
        assert_eq!(
            extra_roots_from(" /a/t : :/b/t"),
            vec![PathBuf::from("/a/t"), PathBuf::from("/b/t")]
        );
        assert!(extra_roots_from("").is_empty());
    }

    #[test]
    fn two_paths_on_one_filesystem_are_one_reading() {
        let probes = [
            ("/".to_string(), 1u64, 26.0),
            ("the state dir".to_string(), 1, 26.0),
            ("the gate target".to_string(), 2, 54.0),
            ("the debug target".to_string(), 3, 4.0),
            ("the release target".to_string(), 1, 26.0),
        ];
        let merged = merge_by_device(&probes);
        assert_eq!(merged.len(), 3);
        assert_eq!(merged[0].label(), "/ and the state dir and the release target");
        assert_eq!(merged[2].label(), "the debug target");
        assert_eq!(merged[2].free_gb, 4.0);
    }

    #[test]
    fn a_fast_drop_is_reported_and_slow_or_stale_ones_are_not() {
        use chrono::{Duration as D, TimeZone, Utc};
        let at = Utc.with_ymd_and_hms(2026, 10, 6, 12, 0, 0).unwrap();
        let reading = |dev: u64, free: f64| FsReading { labels: vec![format!("fs{dev}")], dev, free_gb: free };
        let past = |free: f64, hours: i64| PastReading { free_gb: free, at: at - D::hours(hours) };
        let prev: HashMap<String, PastReading> = [
            ("1".to_string(), past(90.0, 2)),  // lost 40 GB in 2 h
            ("2".to_string(), past(60.0, 2)),  // lost 5 GB: ordinary
            ("3".to_string(), past(90.0, 200)), // lost 40 GB over 8 days: no baseline
            ("4".to_string(), past(10.0, 2)),  // gained space
        ]
        .into();
        let now = [reading(1, 50.0), reading(2, 55.0), reading(3, 50.0), reading(4, 80.0), reading(5, 1.0)];
        let drops = fs_drops(&prev, &now, at, 20.0, 6.0);
        assert_eq!(drops.len(), 1, "{drops:?}");
        assert_eq!(drops[0].label, "fs1");
        assert_eq!(drops[0].lost_gb, 40.0);
        assert_eq!(drops[0].hours, 2.0);
    }

    #[test]
    fn a_symlinked_debug_target_is_watched_where_it_really_lives() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("volume/proj-debug");
        std::fs::create_dir_all(&real).unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join("target")).unwrap();
        std::os::unix::fs::symlink(&real, repo.join("target/debug")).unwrap();
        let watched = watched_paths(&repo);
        let debug = watched.iter().find(|(l, _)| l == "the debug target").expect("debug watched");
        assert_eq!(debug.1, std::fs::canonicalize(&real).unwrap());
        for label in ["/", "the temp dir", "the gate target", "the release target"] {
            assert!(watched.iter().any(|(l, _)| l == label), "{label} not watched");
        }
    }

    // ---- filesystem-backed: a synthetic profile dir, never a real build ----

    fn age(path: &Path, days: f64) {
        let t = SystemTime::now() - Duration::from_secs_f64(days * DAY);
        let f = std::fs::File::options().write(true).open(path).or_else(|_| std::fs::File::open(path)).unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(t).set_accessed(t)).unwrap();
    }

    fn write(path: &Path, bytes: usize) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, vec![7u8; bytes]).unwrap();
    }

    /// One unit as cargo lays it out: fingerprint dir (hash file + json with
    /// its dependency fingerprints) and an rlib in `deps/`, all `days` old.
    fn write_unit(profile: &Path, hash: &str, fp: u64, deps: &[u64], days: f64) {
        let dir = profile.join(format!(".fingerprint/pkg-{hash}"));
        std::fs::create_dir_all(&dir).unwrap();
        let rows: Vec<String> = deps.iter().map(|d| format!("[1,\"dep\",false,{d}]")).collect();
        std::fs::write(dir.join("lib-pkg"), fingerprint_hex(fp)).unwrap();
        std::fs::write(dir.join("lib-pkg.json"), format!("{{\"deps\":[{}]}}", rows.join(","))).unwrap();
        let rlib = profile.join(format!("deps/libpkg-{hash}.rlib"));
        write(&rlib, 4096);
        for f in [dir.join("lib-pkg"), dir.join("lib-pkg.json"), dir.clone(), rlib] {
            age(&f, days);
        }
    }

    const APP: &str = "a000000000000001";
    const WARM_DEP: &str = "b000000000000002";
    const COLD: &str = "c000000000000003";
    const ORPHAN: &str = "d000000000000004";

    /// `debug/` holding: an app compiled today that links a dependency
    /// compiled 30 days ago; a unit nothing has needed for 30 days; an rlib
    /// with no fingerprint; two incremental sessions; the top-level binary.
    fn synthetic_profile(root: &Path) -> PathBuf {
        let p = root.join("debug");
        write_unit(&p, APP, 1, &[2], 0.0);
        write_unit(&p, WARM_DEP, 2, &[], 30.0);
        write_unit(&p, COLD, 3, &[], 30.0);
        let orphan = p.join(format!("deps/libgone-{ORPHAN}.rlib"));
        write(&orphan, 4096);
        age(&orphan, 30.0);
        for (name, days) in [("old-sess", 5.0), ("new-sess", 0.0)] {
            let f = p.join(format!("incremental/{name}/s-x/query-cache.bin"));
            write(&f, 4096);
            age(&f, days);
            age(f.parent().unwrap(), days);
            age(f.parent().unwrap().parent().unwrap(), days);
        }
        write(&p.join("augmentagent"), 4096);
        age(&p.join("augmentagent"), 90.0);
        p
    }

    fn cold_names() -> Vec<String> {
        let mut v = vec![
            format!("libgone-{ORPHAN}.rlib"),
            format!("libpkg-{COLD}.rlib"),
            "old-sess".to_string(),
            format!("pkg-{COLD}"),
        ];
        v.sort();
        v
    }

    fn names(cands: &[Candidate]) -> Vec<String> {
        let mut v: Vec<String> = cands
            .iter()
            .map(|c| c.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn an_old_artifact_survives_while_a_recent_compile_depends_on_it() {
        let tmp = tempfile::tempdir().unwrap();
        let profile = synthetic_profile(tmp.path());
        let (cands, understood) = scan_profile(&profile, SystemTime::now());
        assert!(understood);
        assert_eq!(
            names(&select_idle(&cands, &Policy::default())),
            cold_names(),
            "the 30-day-old dependency of today's build must survive; the equally old \
             unit nothing depends on must not"
        );
    }

    #[test]
    fn unreadable_fingerprint_records_keep_every_artifact() {
        let tmp = tempfile::tempdir().unwrap();
        let profile = synthetic_profile(tmp.path());
        // Point every dependency at fingerprints no unit has.
        for hash in [APP, WARM_DEP, COLD] {
            let json = profile.join(format!(".fingerprint/pkg-{hash}/lib-pkg.json"));
            std::fs::write(&json, "{\"deps\":[[1,\"dep\",false,777],[1,\"dep\",false,778]]}").unwrap();
            age(&json, 30.0);
        }
        let (cands, understood) = scan_profile(&profile, SystemTime::now());
        assert!(!understood);
        assert_eq!(names(&select_idle(&cands, &Policy::default())), ["old-sess"]);
    }

    #[test]
    fn the_scan_never_offers_a_top_level_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let profile = synthetic_profile(tmp.path());
        let (cands, _) = scan_profile(&profile, SystemTime::now());
        assert!(
            cands.iter().all(|c| c.path.parent() != Some(profile.as_path())),
            "the deployed binary lives at the top of the profile dir"
        );
    }

    #[test]
    fn prune_removes_cold_entries_and_keeps_warm_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let profile = synthetic_profile(tmp.path());
        let root = Root { gate: false, orphan: false, ..Root::at("t", tmp.path()) };
        let opts = Options::default();
        let dry = prune_root(
            &root,
            None,
            &Options { dry_run: true, ..opts.clone() },
            &Policy::default(),
            SystemTime::now(),
        );
        assert_eq!(dry.removed, 4);
        assert!(profile.join(format!("deps/libpkg-{COLD}.rlib")).exists(), "dry run removes nothing");

        let real = prune_root(&root, None, &opts, &Policy::default(), SystemTime::now());
        assert_eq!(real.removed, 4);
        assert!(real.freed_bytes > 0);
        assert!(!profile.join(format!("deps/libpkg-{COLD}.rlib")).exists());
        assert!(!profile.join(format!(".fingerprint/pkg-{COLD}")).exists());
        assert!(!profile.join("incremental/old-sess").exists());
        assert!(profile.join(format!("deps/libpkg-{WARM_DEP}.rlib")).exists());
        assert!(profile.join(format!("deps/libpkg-{APP}.rlib")).exists());
        assert!(profile.join("incremental/new-sess").exists());
        assert!(profile.join("augmentagent").exists());
    }

    #[test]
    fn a_busy_root_is_left_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let profile = synthetic_profile(tmp.path());
        let root = Root { gate: false, orphan: false, ..Root::at("t", tmp.path()) };
        let r = prune_root(
            &root,
            Some("a cargo build is using it"),
            &Options::default(),
            &Policy::default(),
            SystemTime::now(),
        );
        assert_eq!(r.removed, 0);
        assert!(r.outcome.starts_with("skipped"), "{}", r.outcome);
        assert!(profile.join("incremental/old-sess").exists());
    }

    #[test]
    fn an_orphaned_worktree_target_goes_whole_only_once_it_is_quiet() {
        let tmp = tempfile::tempdir().unwrap();
        let profile = synthetic_profile(tmp.path());
        let root = Root { gate: false, orphan: true, ..Root::at("wt", tmp.path()) };
        // Written minutes ago: someone may be building in a worktree parked
        // on main. Falls through to the ordinary age rules.
        let r = prune_root(&root, None, &Options { dry_run: true, ..Default::default() }, &Policy::default(), SystemTime::now());
        assert!(!r.full_drop, "{r:?}");

        for d in ["deps", ".fingerprint", "incremental"] {
            age(&profile.join(d), 3.0);
        }
        let r = prune_root(&root, None, &Options::default(), &Policy::default(), SystemTime::now());
        assert!(r.full_drop);
        assert!(!tmp.path().exists(), "the whole target dir is gone");
    }

    #[test]
    fn a_cap_trim_that_cannot_fit_drops_the_profile_but_keeps_the_root() {
        let tmp = tempfile::tempdir().unwrap();
        let profile = synthetic_profile(tmp.path());
        std::fs::create_dir_all(tmp.path().join("tmp")).unwrap();
        let root = Root { gate: true, orphan: false, ..Root::at("gate", tmp.path()) };
        let opts = Options { cap_mb: Some(0), ..Default::default() };
        let r = prune_root(&root, None, &opts, &Policy::default(), SystemTime::now());
        assert!(r.full_drop);
        assert!(!profile.exists());
        assert!(tmp.path().join("tmp").exists(), "the gate's TMPDIR parent survives");
    }

    #[test]
    fn a_symlinked_profile_is_pruned_through_the_link_and_its_parent_is_never_a_root() {
        // `target/debug -> <other volume>/proj-debug`, next to a stranger's
        // data on that volume.
        let tmp = tempfile::tempdir().unwrap();
        let elsewhere = tmp.path().join("volume");
        let real = synthetic_profile(&elsewhere);
        let stranger = elsewhere.join("someone-else/deps/libx-dddddddddddddddd.rlib");
        write(&stranger, 4096);
        age(&stranger, 90.0);
        let target = tmp.path().join("repo/target");
        std::fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(&real, target.join("debug")).unwrap();

        let root = Root::at("checkout", &std::fs::canonicalize(&target).unwrap());
        assert_eq!(root.profiles, vec![std::fs::canonicalize(&real).unwrap()]);
        let r = prune_root(&root, None, &Options::default(), &Policy::default(), SystemTime::now());
        assert_eq!(r.removed, 4);
        assert!(!real.join("incremental/old-sess").exists());
        assert!(stranger.exists(), "a sibling of the link target is not ours to prune");
    }

    #[test]
    fn a_held_flock_reads_as_held_and_a_missing_file_does_not() {
        use std::os::unix::io::AsRawFd;
        let tmp = tempfile::tempdir().unwrap();
        let lock = tmp.path().join("lane.lock");
        assert!(!lock_is_held(&lock), "missing file");
        let holder = std::fs::File::create(&lock).unwrap();
        assert!(!lock_is_held(&lock), "exists but nobody holds it");
        assert_eq!(unsafe { libc::flock(holder.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
        assert!(lock_is_held(&lock));
        drop(holder);
        assert!(!lock_is_held(&lock));
    }
}
