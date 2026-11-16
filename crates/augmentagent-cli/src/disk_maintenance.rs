//! Serialize maintenance passes without excluding unrelated managed writers.
use std::os::fd::AsRawFd;
use std::path::Path;

use anyhow::Result;

pub(super) struct Lease(std::fs::File);

impl Lease {
    pub(super) fn try_at(state: &Path) -> Result<Option<Self>> {
        std::fs::create_dir_all(state)?;
        // Keep the mandatory abandoned-CI reconciliation fence. This short
        // lock is released before target acquisition; target leases never wait.
        let _admission = crate::storage_reservations::Admission::lock()?;
        let file = augmentagent_channel_core::storage_coordination::open_private(
            &state.join("disk-prune.lock"), false)?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            Ok(Some(Self(file)))
        } else {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock { Ok(None) }
            else { Err(error.into()) }
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maintenance_serializes_without_waiting_for_writers() {
        let state = tempfile::tempdir().unwrap();
        let _writer = crate::build_storage::Lease::try_at(state.path()).unwrap().unwrap();
        let first = Lease::try_at(state.path()).unwrap().unwrap();
        assert!(Lease::try_at(state.path()).unwrap().is_none());
        drop(first);
        assert!(Lease::try_at(state.path()).unwrap().is_some());
        assert!(crate::build_storage::Lease::try_at(state.path()).unwrap().is_none());
    }
}
