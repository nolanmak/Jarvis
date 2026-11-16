//! Managed relocation changes future destinations; it never copies artifacts.
use crate::{build_storage, disk, storage_leases::TargetLeases};
use anyhow::{ensure, Context, Result};
use augmentagent_channel_core::build_volume;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::{
    io::Write,
    path::{Path, PathBuf},
};

pub(crate) struct Filesystem {
    pub uid: u32,
    pub device: u64,
    pub free: u64,
    pub writable: bool,
}
pub(crate) trait Probe {
    fn inspect(&self, path: &Path) -> Result<Filesystem>;
}
struct Native;
impl Probe for Native {
    fn inspect(&self, path: &Path) -> Result<Filesystem> {
        use std::os::unix::ffi::OsStrExt;
        let existing = path
            .ancestors()
            .find(|p| p.exists())
            .context("relocate_filesystem_unavailable")?;
        let meta = std::fs::metadata(existing)?;
        let path = std::ffi::CString::new(existing.as_os_str().as_bytes())?;
        Ok(Filesystem {
            uid: meta.uid(),
            device: meta.dev(),
            free: disk::available_bytes(existing).context("relocate_headroom_unknown")?,
            writable: unsafe { libc::access(path.as_ptr(), libc::W_OK | libc::X_OK) } == 0,
        })
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Retirement {
    version: u32,
    repo: PathBuf,
    path: PathBuf,
    device: u64,
    inode: u64,
    marker: Option<String>,
}
#[derive(Serialize)]
pub(crate) struct Plan {
    pub volume: PathBuf,
    pub source_paths: Vec<PathBuf>,
    pub outputs: build_volume::Paths,
    pub retire: Vec<PathBuf>,
    pub busy: Vec<PathBuf>,
    pub available_bytes: u64,
    pub required_bytes: u64,
    pub configuration: String,
    pub dry_run: bool,
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn marker(path: &Path) -> Result<Option<String>> {
    let path = path.join(".augmentagent-target.json");
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    ensure!(
        file.metadata()?.is_file() && file.metadata()?.len() <= 65536,
        "relocate_invalid_cache_marker"
    );
    use std::io::Read;
    let mut bytes = Vec::new();
    file.take(65537).read_to_end(&mut bytes)?;
    Ok(Some(hash(&bytes)))
}
fn record(repo: &Path, path: &Path) -> Result<Retirement> {
    let meta = std::fs::symlink_metadata(path)?;
    ensure!(
        meta.is_dir() && !meta.file_type().is_symlink() && meta.uid() == unsafe { libc::geteuid() },
        "relocate_cache_not_owned"
    );
    Ok(Retirement {
        version: 1,
        repo: build_storage::repo_id(repo)?,
        path: path.canonicalize()?,
        device: meta.dev(),
        inode: meta.ino(),
        marker: marker(path)?,
    })
}
fn protected_future(path: &Path, outputs: &build_volume::Paths) -> bool {
    let resolved = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let explicit = std::env::var_os("CARGO_TARGET_DIR")
        .filter(|p| !p.is_empty())
        .and_then(|p| std::path::absolute(PathBuf::from(p)).ok());
    path == resolved(&outputs.gate)
        || path.starts_with(resolved(&outputs.sessions))
        || path.starts_with(resolved(&outputs.verifiers))
        || explicit.is_some_and(|p| path == resolved(&p))
}
fn validate_destination(to: &Path, probe: &dyn Probe) -> Result<PathBuf> {
    let absolute = std::path::absolute(to)?;
    let meta = std::fs::symlink_metadata(&absolute).context("relocate_destination_missing")?;
    ensure!(
        !meta.file_type().is_symlink() && absolute.canonicalize()? == absolute,
        "relocate_destination_symlink"
    );
    ensure!(meta.is_dir(), "relocate_destination_not_directory");
    let fs = probe.inspect(&absolute)?;
    ensure!(
        fs.uid == unsafe { libc::geteuid() },
        "relocate_destination_not_owned"
    );
    ensure!(fs.writable, "relocate_destination_not_writable");
    Ok(absolute)
}
fn plan(
    repo: &Path,
    to: &Path,
    dry_run: bool,
    probe: &dyn Probe,
) -> Result<(Plan, Vec<Retirement>)> {
    let volume = validate_destination(to, probe)?;
    let destination = probe.inspect(&volume)?;
    let state = build_storage::state_dir()?;
    let owned = build_storage::registered(repo, &state);
    let current = build_volume::paths();
    let mut sources = owned.iter().map(|t| t.path.clone()).collect::<Vec<_>>();
    sources.extend(build_storage::managed_parents(repo, &state));
    sources.extend([
        current.gate,
        current.sessions,
        current.verifiers,
        current.scratch,
    ]);
    sources.sort();
    sources.dedup();
    let sources = sources
        .into_iter()
        .filter(|p| {
            probe
                .inspect(p)
                .is_ok_and(|s| s.device != destination.device)
        })
        .collect::<Vec<_>>();
    ensure!(!sources.is_empty(), "relocate_same_filesystem");
    let required = (build_storage::admission_floor_gb() * 1024_f64.powi(3)).ceil() as u64;
    ensure!(
        destination.free >= required,
        "relocate_insufficient_headroom: {} available < {} required on {}",
        destination.free,
        required,
        volume.display()
    );
    let outputs = build_volume::for_volume(Some(&volume));
    let mut retired = Vec::new();
    let mut busy = Vec::new();
    let procs = disk::build_procs();
    for target in owned {
        if protected_future(&target.path, &outputs)
            || probe.inspect(&target.path)?.device == destination.device
        {
            continue;
        }
        if let Ok(value) = record(repo, &target.path) {
            retired.push(value);
        }
        // Dry-run must not even create target-lock files. Existing lease state
        // is checked during application/prune, not by mutating a plan.
        if procs
            .as_ref()
            .is_some_and(|procs| disk::root_in_use(&target.path, procs, &|p| p.canonicalize().ok()))
        {
            busy.push(target.path);
        }
    }
    let report = Plan {
        configuration: format!(
            "AUGMENTAGENT_BUILD_VOLUME={}",
            serde_json::to_string(&volume)?
        ),
        volume,
        source_paths: sources,
        outputs,
        retire: retired.iter().map(|r| r.path.clone()).collect(),
        busy,
        available_bytes: destination.free,
        required_bytes: required,
        dry_run,
    };
    Ok((report, retired))
}
fn atomic(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut file =
        tempfile::NamedTempFile::new_in(path.parent().context("relocate_record_parent")?)?;
    file.as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(&serde_json::to_vec(value)?)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|e| e.error)?;
    std::fs::File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}
pub(crate) fn run(repo: &Path, to: &Path, dry_run: bool, out: &mut dyn Write) -> Result<()> {
    run_with(repo, to, dry_run, out, &Native)
}
pub(crate) fn run_with(
    repo: &Path,
    to: &Path,
    dry_run: bool,
    out: &mut dyn Write,
    probe: &dyn Probe,
) -> Result<()> {
    let (report, retired) = plan(repo, to, dry_run, probe)?;
    if !dry_run {
        // Short metadata admission, never the long global build lock. Cache
        // writers retain their target leases and their artifacts stay in place.
        let _admission = crate::storage_reservations::Admission::lock()?;
        validate_destination(to, probe)?;
        for parent in [&report.outputs.sessions, &report.outputs.verifiers] {
            if !parent.starts_with(&report.volume) {
                continue;
            }
            ensure!(
                !parent
                    .symlink_metadata()
                    .is_ok_and(|m| m.file_type().is_symlink()),
                "relocate_output_symlink"
            );
            build_storage::manage_parent(repo, parent, &build_storage::state_dir()?)?;
        }
        if report.outputs.gate.starts_with(&report.volume) {
            let gate = &report.outputs.gate;
            ensure!(
                !gate
                    .symlink_metadata()
                    .is_ok_and(|m| m.file_type().is_symlink()),
                "relocate_output_symlink"
            );
            std::fs::create_dir_all(gate)?;
            let _lease = TargetLeases::try_directory(gate)?.context("relocate_destination_busy")?;
            build_storage::register(repo, gate, None, &build_storage::state_dir()?)?;
        }
        let records = build_storage::state_dir()?.join("build-relocations");
        std::fs::create_dir_all(&records)?;
        std::fs::set_permissions(&records, std::fs::Permissions::from_mode(0o700))?;
        for value in retired {
            atomic(
                &records.join(format!(
                    "{}.json",
                    hash(value.path.as_os_str().as_encoded_bytes())
                )),
                &value,
            )?;
        }
    }
    writeln!(out, "{}", serde_json::to_string_pretty(&report)?)?;
    Ok(())
}

/// Wiring into the normal pruner retains its existing busy checks, physical
/// leases, retention protections and profile-only removal authority.
pub(crate) fn retired_options(
    repo: &Path,
    root: &disk::Root,
    opts: &mut disk::Options,
) -> Result<()> {
    let state = build_storage::state_dir()?;
    let path = state.join("build-relocations").join(format!(
        "{}.json",
        hash(root.path.as_os_str().as_encoded_bytes())
    ));
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let meta = file.metadata()?;
    ensure!(
        meta.is_file() && meta.uid() == unsafe { libc::geteuid() } && meta.len() <= 65536,
        "relocate_invalid_retirement"
    );
    use std::io::Read;
    let mut raw = Vec::new();
    file.take(65537).read_to_end(&mut raw)?;
    ensure!(raw.len() <= 65536, "relocate_invalid_retirement");
    let recorded: Retirement = serde_json::from_slice(&raw)?;
    if recorded.version != 1
        || recorded.repo != build_storage::repo_id(repo)?
        || recorded.path != root.path
    {
        return Ok(());
    }
    if protected_future(&root.path, &build_volume::paths()) {
        return Ok(());
    }
    let Ok(actual) = record(repo, &root.path) else {
        return Ok(());
    };
    if actual.device != recorded.device
        || actual.inode != recorded.inode
        || actual.marker != recorded.marker
    {
        return Ok(());
    }
    if !build_storage::registered(repo, &state)
        .iter()
        .any(|t| t.path == root.path)
    {
        return Ok(());
    }
    opts.cap_mb = Some(0);
    opts.stale_days = Some(0.0);
    Ok(())
}
