//! Cross-process allocation accounting. Writers hold their reservation flock
//! until all subprocesses stop. The shared admission lock makes measuring
//! space and publishing a new reservation atomic across XDG state directories.
//! #1930 — this ledger is shared: the CLI build gate and the Autoship capacity
//! paths outside it publish into one set of claims, so neither can hand out
//! headroom the other already reserved.
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::storage_coordination::{self as coordination, lock_directory, open_private};

pub use crate::storage_coordination::Busy;

/// The growth-budget ledger layered on the shared admission lock: holding this
/// is proof that the one storage admission, and its CI cleanup reconciliation,
/// belong to this writer.
pub struct Admission {
    /// Held until this value drops, which releases the shared lock.
    _shared: coordination::Admission,
}
impl Admission {
    pub fn lock() -> Result<Self> {
        Ok(Self {
            _shared: coordination::Admission::lock()?,
        })
    }

    pub fn claims(&self, exclude: Option<&str>) -> Result<Claims> {
        let mut claims = Vec::new();
        for entry in std::fs::read_dir(lock_directory()?)? {
            let path = entry?.path();
            if path.extension().and_then(|s| s.to_str()) != Some("reservation") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if Some(id) == exclude {
                continue;
            }
            if uuid::Uuid::parse_str(id).is_err() {
                bail!("invalid storage reservation name");
            }
            let mut file = match open_private(&path, false) {
                Ok(file) => file,
                Err(_) if !path.exists() => continue,
                Err(error) => return Err(error),
            };
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                // The unique filename is never reused. A crashed writer's
                // stale claim cannot keep reserving space indefinitely.
                let _ = std::fs::remove_file(&path);
                continue;
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::WouldBlock {
                return Err(error.into());
            }
            let mut bytes = Vec::new();
            Read::by_ref(&mut file)
                .take(65537)
                .read_to_end(&mut bytes)?;
            if bytes.len() > 65536 {
                bail!("oversized storage reservation");
            }
            let record: Claim =
                serde_json::from_slice(&bytes).context("unreadable active storage reservation")?;
            if record.version != 1 {
                bail!("unknown storage reservation version");
            }
            claims.push(record);
        }
        Ok(Claims(claims))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Claim {
    version: u8,
    state: String,
    kind: Kind,
    devices: Vec<u64>,
    growth: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
enum Kind {
    Normal,
    Update,
    Snapshot,
}

#[derive(Default)]
pub struct Claims(Vec<Claim>);
impl Claims {
    pub fn reserved_on(&self, path: &Path) -> Result<u64> {
        let device = device(path)?;
        Ok(self.reserved_on_device(device))
    }
    fn reserved_on_device(&self, device: u64) -> u64 {
        self.0
            .iter()
            .filter(|r| r.devices.contains(&device))
            .fold(0u64, |sum, r| sum.saturating_add(r.growth))
    }
    pub fn has_normal_writer(&self, state: &Path) -> Result<bool> {
        let state = state_key(state)?;
        Ok(self
            .0
            .iter()
            .any(|r| r.kind == Kind::Normal && r.state == state))
    }
}

fn state_key(state: &Path) -> Result<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(std::fs::canonicalize(state)?.as_os_str().as_encoded_bytes())
    ))
}

fn device(path: &Path) -> Result<u64> {
    for ancestor in path.ancestors() {
        match std::fs::metadata(ancestor) {
            Ok(meta) => return Ok(meta.dev()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        }
    }
    bail!("cannot determine storage filesystem for {}", path.display())
}

pub struct Reservation {
    id: String,
    path: PathBuf,
    file: File,
    claim: Claim,
    exported: std::sync::atomic::AtomicBool,
}
impl Reservation {
    pub fn new(_admission: &Admission, state: &Path, update: bool) -> Result<Self> {
        Self::new_kind(state, if update { Kind::Update } else { Kind::Normal })
    }
    pub fn snapshot(_admission: &Admission, root: &Path) -> Result<Self> {
        Self::new_kind(root, Kind::Snapshot)
    }
    fn new_kind(state: &Path, kind: Kind) -> Result<Self> {
        let id = uuid::Uuid::new_v4().to_string();
        let path = Self::path_for(&id)?;
        let file = open_private(&path, true)?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut result = Self {
            id,
            path,
            file,
            exported: std::sync::atomic::AtomicBool::new(false),
            claim: Claim {
                version: 1,
                state: state_key(state)?,
                kind,
                devices: vec![],
                growth: 0,
            },
        };
        result.write()?;
        Ok(result)
    }
    pub fn inherited_file(&self) -> Result<File> {
        let file=self.uncommitted_file()?;
        self.commit_export();
        Ok(file)
    }
    /// Duplicate for a not-yet-started worker without changing Drop behavior.
    pub fn uncommitted_file(&self)->Result<File> {Ok(self.file.try_clone()?)}
    /// Preserve the reservation until the worker's final descriptor closes.
    pub fn commit_export(&self) {
        self.exported.store(true,std::sync::atomic::Ordering::Release);
    }
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn fd(&self) -> i32 {
        self.file.as_raw_fd()
    }
    pub fn path_for(id: &str) -> Result<PathBuf> {
        uuid::Uuid::parse_str(id).context("invalid parent storage reservation")?;
        Ok(lock_directory()?.join(format!("{id}.reservation")))
    }
    pub fn proof_mode(id: &str, state: &Path) -> Result<bool> {
        let claim: Claim = serde_json::from_slice(&std::fs::read(Self::path_for(id)?)?)?;
        if claim.version != 1 || claim.state != state_key(state)? {
            bail!("parent reservation does not cover this application state");
        }
        Ok(claim.kind == Kind::Update)
    }
    pub fn publish(
        &mut self,
        _admission: &Admission,
        paths: &[PathBuf],
        growth: u64,
    ) -> Result<()> {
        let mut devices = paths
            .iter()
            .map(|p| device(p))
            .collect::<Result<Vec<_>>>()?;
        devices.sort_unstable();
        devices.dedup();
        self.claim.devices = devices;
        self.claim.growth = growth;
        self.write()
    }
    fn write(&mut self) -> Result<()> {
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&serde_json::to_vec(&self.claim)?)?;
        self.file.flush()?;
        Ok(())
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        // Exported claims remain visible until the final worker closes them.
        // Admission::claims reclaims the record once its flock becomes free.
        if !self.exported.load(std::sync::atomic::Ordering::Acquire) {
            unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #1930 — a writer outside the CLI binary publishes into the same ledger
    /// the build gate reads, so its growth is charged against one budget.
    #[test]
    fn claims_published_outside_the_cli_are_visible_to_every_writer() {
        let root = tempfile::tempdir().unwrap();
        let admission = Admission::lock().unwrap();
        let mut reservation = Reservation::new(&admission, root.path(), false).unwrap();
        reservation
            .publish(&admission, &[root.path().to_path_buf()], 4096)
            .unwrap();
        assert!(reservation.path.starts_with(lock_directory().unwrap()));
        let claims = admission.claims(None).unwrap();
        assert!(claims.reserved_on(root.path()).unwrap() >= 4096);
        assert!(claims.has_normal_writer(root.path()).unwrap());
    }

    #[test]
    fn snapshot_peak_is_reserved_without_attesting_a_normal_writer() {
        let root = tempfile::tempdir().unwrap();
        let admission = Admission::lock().unwrap();
        let mut snapshot = Reservation::snapshot(&admission, root.path()).unwrap();
        snapshot
            .publish(&admission, &[root.path().to_path_buf()], 1234)
            .unwrap();
        let claims = admission.claims(None).unwrap();
        assert!(claims.reserved_on(root.path()).unwrap() >= 1234);
        assert!(!claims.has_normal_writer(root.path()).unwrap());
    }

    #[test]
    fn reservations_charge_each_shared_device_once() {
        let claims = Claims(vec![
            Claim {
                version: 1,
                state: "one".into(),
                kind: Kind::Normal,
                devices: vec![10, 20],
                growth: 100,
            },
            Claim {
                version: 1,
                state: "two".into(),
                kind: Kind::Update,
                devices: vec![20],
                growth: 200,
            },
        ]);
        assert_eq!(claims.reserved_on_device(10), 100);
        assert_eq!(claims.reserved_on_device(20), 300);
        assert_eq!(claims.reserved_on_device(30), 0);
    }
    #[test]
    fn exported_reservation_survives_controller_drop_until_worker_closes() {
        let root = tempfile::tempdir().unwrap();
        let admission = Admission::lock().unwrap();
        let mut reservation = Reservation::new(&admission, root.path(), false).unwrap();
        reservation.publish(&admission, &[root.path().to_path_buf()], 1234).unwrap();
        let path = reservation.path.clone();
        let worker = reservation.inherited_file().unwrap();
        drop(reservation);
        assert!(path.exists());
        assert!(admission.claims(None).unwrap().has_normal_writer(root.path()).unwrap());
        drop(worker);
        assert!(!admission.claims(None).unwrap().has_normal_writer(root.path()).unwrap());
        assert!(!path.exists());
    }

    #[test]
    fn claims_span_state_directories_and_end_with_the_writer() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let admission = Admission::lock().unwrap();
        let mut normal = Reservation::new(&admission, a.path(), false).unwrap();
        let mut update = Reservation::new(&admission, b.path(), true).unwrap();
        normal
            .publish(&admission, &[a.path().to_path_buf()], 111)
            .unwrap();
        update
            .publish(&admission, &[b.path().to_path_buf()], 222)
            .unwrap();
        let claims = admission.claims(Some(update.id())).unwrap();
        assert!(claims.has_normal_writer(a.path()).unwrap());
        // Other concurrently running tests may own claims; check our own ID
        // and writer removal rather than relying on an empty global ledger.
        assert!(claims.reserved_on(a.path()).unwrap() >= 111);
        let path = normal.path.clone();
        drop(normal);
        assert!(!path.exists());
        assert!(!admission
            .claims(None)
            .unwrap()
            .has_normal_writer(a.path())
            .unwrap());
    }
}
