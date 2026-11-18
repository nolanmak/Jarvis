//! Read committed custody facts under the existing writer lock, without sealing.
use super::*;
impl AnchoredAudit {
    pub(crate) fn inspect_committed<T>(
        &self,
        read: impl FnOnce(&[serde_json::Value]) -> Result<T>,
    ) -> Result<T> {
        self.inspect_snapshot(|_, rows| read(rows))
    }
    pub(crate) fn sealed_custody_proof(&self) -> Result<Option<serde_json::Value>> {
        self.inspect_snapshot(|anchor, _| Ok(anchor.sealed.clone()))
    }
    fn inspect_snapshot<T>(
        &self,
        read: impl FnOnce(&Anchor, &[serde_json::Value]) -> Result<T>,
    ) -> Result<T> {
        let lock = private_open(&self.lock_path()?, false)?;
        #[cfg(unix)]
        ensure!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } == 0,
            "audit lock failed"
        );
        ensure!(
            self.anchor.exists() && self.log.exists(),
            "audit inventory has no committed evidence"
        );
        let file = private_open(&self.anchor, false)?;
        ensure!(file.metadata()?.len() <= LIMIT, "anchor exceeds budget");
        let anchor: Anchor = serde_json::from_reader(file)?;
        ensure!(
            anchor.version == 1 && anchor.pending.is_none(),
            "audit inventory has an incomplete append"
        );
        let mut file = private_open(&self.log, false)?;
        ensure!(file.metadata()?.len() <= LIMIT, "audit exceeds budget");
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() == anchor.bytes,
            "audit inventory has uncommitted or missing bytes"
        );
        let actual = if bytes.is_empty() {
            String::new()
        } else {
            digest(&bytes)
        };
        ensure!(actual == anchor.hash, "committed audit was modified");
        let rows = bytes
            .split(|b| *b == b'\n')
            .filter(|row| !row.is_empty())
            .map(serde_json::from_slice)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        read(&anchor, &rows)
    }
}
