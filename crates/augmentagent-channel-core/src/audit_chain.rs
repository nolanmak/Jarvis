//! Daemon-owned per-workflow audit chain. The anchor must live outside every
//! worker write scope. This detects corruption, not compromise of the daemon.
//! Deliberately no retention: archive the complete workflow and its anchor.
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::os::{
    fd::AsRawFd,
    unix::fs::{MetadataExt, OpenOptionsExt},
};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

const LIMIT: u64 = 64 * 1024 * 1024;
#[derive(Clone, Debug)]
pub struct AnchoredAudit {
    log: PathBuf,
    anchor: PathBuf,
}
#[derive(Default, Serialize, Deserialize)]
struct Anchor {
    version: u32,
    bytes: usize,
    sequence: u64,
    hash: String,
    pending: Option<String>,
    #[serde(default)]
    sealed: Option<serde_json::Value>,
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn private_open(path: &Path, append: bool) -> Result<File> {
    let mut opts = OpenOptions::new();
    opts.read(true).write(true).create(true).append(append);
    #[cfg(unix)]
    opts.mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let file = opts.open(path)?;
    let meta = file.metadata()?;
    ensure!(meta.is_file(), "audit path is not a regular file");
    #[cfg(unix)]
    ensure!(
        meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o077 == 0 && meta.nlink() == 1,
        "audit file is not owner-private"
    );
    Ok(file)
}
fn canonical_destination(path: &Path) -> Result<PathBuf> {
    let parent = path
        .parent()
        .context("audit destination needs parent")?
        .canonicalize()?;
    let name = path
        .file_name()
        .context("audit destination needs filename")?;
    let path = parent.join(name);
    if let Ok(meta) = fs::symlink_metadata(&path) {
        ensure!(!meta.file_type().is_symlink(), "audit symlink refused");
    }
    Ok(path)
}
impl AnchoredAudit {
    /// Parents must already exist and be daemon-owned. Pass the complete set
    /// of admitted worker write roots, including runtime and build caches.
    pub fn new(log: PathBuf, anchor: PathBuf, writable_roots: &[PathBuf]) -> Result<Self> {
        let log = canonical_destination(&log)?;
        let anchor = canonical_destination(&anchor)?;
        ensure!(
            log.parent() != anchor.parent(),
            "anchor must be external to log directory"
        );
        for root in writable_roots {
            let root = root.canonicalize()?;
            ensure!(
                !log.starts_with(&root) && !anchor.starts_with(&root),
                "worker can mutate audit storage"
            );
        }
        Ok(Self { log, anchor })
    }
    fn save(&self, anchor: &Anchor) -> Result<()> {
        let parent = self.anchor.parent().unwrap();
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer(temp.as_file_mut(), anchor)?;
        temp.as_file().sync_all()?;
        temp.persist(&self.anchor).map_err(|e| e.error)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    }
    /// Appends under a cross-process lock; a durable pending intent permits
    /// replay after a daemon crash without accepting loss of committed rows.
    pub fn append(&self, record: serde_json::Value) -> Result<()> {
        let lock_path = self.anchor.with_extension("lock");
        ensure!(
            lock_path != self.anchor && lock_path != self.log,
            "audit lock path collision"
        );
        let lock = private_open(&lock_path, false)?;
        #[cfg(unix)]
        ensure!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } == 0,
            "audit lock failed"
        );
        self.append_locked(record)
    }
    /// Validate without replaying uncertain records, then atomically prevent all
    /// future appends. The caller validates kernel quiescence while holding the
    /// same lock as writers. A retry returns the identical immutable proof.
    pub fn seal_for_recovery<F>(&self, validate: F) -> Result<serde_json::Value>
    where F: FnOnce(&[serde_json::Value]) -> Result<serde_json::Value> {
        let lock = private_open(&self.anchor.with_extension("lock"), false)?;
        #[cfg(unix)]
        ensure!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } == 0, "audit lock failed");
        ensure!(self.anchor.exists() && self.log.exists(), "recovery requires existing audit evidence");
        let file = private_open(&self.anchor, false)?;
        ensure!(file.metadata()?.len() <= LIMIT, "anchor exceeds budget");
        let mut anchor: Anchor = serde_json::from_reader(file)?;
        ensure!(anchor.version == 1, "unsupported anchor");
        let mut file = private_open(&self.log, false)?;
        ensure!(file.metadata()?.len() <= LIMIT, "audit exceeds budget");
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        ensure!(bytes.len() >= anchor.bytes, "committed audit truncated");
        let committed = &bytes[..anchor.bytes];
        let actual = if committed.is_empty() { String::new() } else { digest(committed) };
        ensure!(actual == anchor.hash, "committed audit modified");
        let tail = &bytes[anchor.bytes..];
        ensure!(match &anchor.pending { Some(p) => p.as_bytes().starts_with(tail), None => tail.is_empty() }, "unrecognized audit tail");
        let rows = committed.split(|b| *b == b'\n').filter(|r| !r.is_empty())
            .map(serde_json::from_slice).collect::<std::result::Result<Vec<serde_json::Value>, _>>()?;
        let validation = validate(&rows)?;
        if let Some(proof) = anchor.sealed { return Ok(proof); }
        let proof = serde_json::json!({"version":1,"committed_sha256":anchor.hash,
            "committed_bytes":anchor.bytes,"sequence":anchor.sequence,
            "interrupted_append":anchor.pending,"validation":validation});
        anchor.sealed = Some(proof.clone());
        self.save(&anchor)?;
        Ok(proof)
    }
    fn append_locked(&self, mut record: serde_json::Value) -> Result<()> {
        let mut anchor = if self.anchor.exists() {
            let file = private_open(&self.anchor, false)?;
            ensure!(
                file.metadata()?.len() <= LIMIT,
                "anchor exceeds workflow budget"
            );
            serde_json::from_reader::<_, Anchor>(file)?
        } else {
            Anchor {
                version: 1,
                ..Default::default()
            }
        };
        ensure!(anchor.version == 1, "unsupported audit anchor");
        ensure!(anchor.sealed.is_none(), "audit generation is sealed");
        let mut file = private_open(&self.log, true)?;
        File::open(self.log.parent().unwrap())?.sync_all()?;
        ensure!(
            file.metadata()?.len() <= LIMIT,
            "audit exceeds workflow budget"
        );
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        ensure!(bytes.len() >= anchor.bytes, "committed audit was truncated");
        let committed = &bytes[..anchor.bytes];
        // Hash the entire committed prefix, so edits to any old record are
        // detected even if the attacker preserves its claimed chain hash.
        let actual = if committed.is_empty() {
            String::new()
        } else {
            digest(committed)
        };
        ensure!(actual == anchor.hash, "committed audit was modified");
        if let Some(pending) = anchor.pending.take() {
            let suffix = &bytes[anchor.bytes..];
            ensure!(
                pending.as_bytes().starts_with(suffix),
                "unrecognized audit tail"
            );
            file.write_all(&pending.as_bytes()[suffix.len()..])?;
            file.sync_all()?;
            bytes.truncate(anchor.bytes);
            bytes.extend_from_slice(pending.as_bytes());
            anchor.bytes = bytes.len();
            anchor.hash = digest(&bytes);
            anchor.sequence += 1;
            self.save(&anchor)?;
        } else {
            ensure!(bytes.len() == anchor.bytes, "audit has unanchored records");
        }
        let object = record
            .as_object_mut()
            .context("audit record must be an object")?;
        ensure!(
            !object.contains_key("audit_chain"),
            "reserved audit metadata"
        );
        let payload_hash = digest(&serde_json::to_vec(&record)?);
        record.as_object_mut().unwrap().insert(
            "audit_chain".into(),
            serde_json::json!({
                "version": 1, "sequence": anchor.sequence + 1,
                "previous": anchor.hash, "payload_sha256": payload_hash
            }),
        );
        let mut line = serde_json::to_string(&record)?;
        line.push('\n');
        if bytes.len() + line.len() > LIMIT as usize {
            bail!("audit exceeds workflow budget");
        }
        anchor.pending = Some(line.clone());
        self.save(&anchor)?;
        file.write_all(line.as_bytes())?;
        file.sync_all()?;
        bytes.extend_from_slice(line.as_bytes());
        anchor.bytes = bytes.len();
        anchor.hash = digest(&bytes);
        anchor.sequence += 1;
        anchor.pending = None;
        self.save(&anchor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn setup() -> (tempfile::TempDir, AnchoredAudit) {
        let dir = tempfile::tempdir().unwrap();
        for name in ["logs", "anchors", "work"] {
            fs::create_dir(dir.path().join(name)).unwrap();
        }
        let sink = AnchoredAudit::new(
            dir.path().join("logs/run.jsonl"),
            dir.path().join("anchors/run.json"),
            &[dir.path().join("work")],
        )
        .unwrap();
        (dir, sink)
    }
    #[test]
    fn recovery_seal_preserves_interrupted_tail_and_blocks_late_writers() {
        let (_dir, sink) = setup();
        sink.append(serde_json::json!({"kind":"admitted"})).unwrap();
        let mut anchor: Anchor = serde_json::from_slice(&fs::read(&sink.anchor).unwrap()).unwrap();
        anchor.pending = Some("{\"uncertain_result\":true}\n".into());
        sink.save(&anchor).unwrap();
        private_open(&sink.log, true).unwrap().write_all(b"{\"uncertain").unwrap();
        let before = fs::read(&sink.log).unwrap();
        assert!(sink.seal_for_recovery(|_| anyhow::bail!("worker live")).is_err());
        let proof = sink.seal_for_recovery(|rows| {
            assert_eq!(rows.len(), 1);
            Ok(serde_json::json!({"quiescent":true}))
        }).unwrap();
        assert_eq!(fs::read(&sink.log).unwrap(), before);
        assert!(proof["interrupted_append"].as_str().unwrap().contains("uncertain_result"));
        assert!(sink.clone().append(serde_json::json!({"late":true})).is_err());
        assert_eq!(sink.seal_for_recovery(|_| Ok(serde_json::json!({}))).unwrap(), proof);
        let mut corrupted = before;
        corrupted[2] ^= 1;
        fs::write(&sink.log, corrupted).unwrap();
        assert!(sink.seal_for_recovery(|_| Ok(serde_json::json!({}))).is_err());
    }
    #[test]
    fn detects_old_record_rewrite_and_committed_truncation() {
        for truncate in [false, true] {
            let (_dir, sink) = setup();
            sink.append(serde_json::json!({"tool":"Read"})).unwrap();
            let mut data = fs::read(&sink.log).unwrap();
            if truncate {
                data.clear();
            } else {
                data[2] ^= 1;
            }
            fs::write(&sink.log, data).unwrap();
            assert!(sink.append(serde_json::json!({"tool":"Write"})).is_err());
        }
    }
    #[test]
    fn recovers_pending_partial_append_exactly_once() {
        let (_dir, sink) = setup();
        sink.append(serde_json::json!({"tool":"Read"})).unwrap();
        let mut anchor: Anchor = serde_json::from_slice(&fs::read(&sink.anchor).unwrap()).unwrap();
        anchor.pending = Some("{\"recovered\":true}\n".into());
        sink.save(&anchor).unwrap();
        private_open(&sink.log, true)
            .unwrap()
            .write_all(b"{\"recover")
            .unwrap();
        sink.append(serde_json::json!({"tool":"Write"})).unwrap();
        let data = fs::read_to_string(&sink.log).unwrap();
        assert_eq!(data.lines().count(), 3);
        assert_eq!(data.matches("recovered").count(), 1);
    }
    #[test]
    fn rejects_scope_overlap_and_symlink_destinations() {
        let (dir, sink) = setup();
        assert!(AnchoredAudit::new(
            sink.log.clone(),
            sink.anchor.clone(),
            &[dir.path().to_path_buf()]
        )
        .is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&sink.anchor, &sink.log).unwrap();
            assert!(AnchoredAudit::new(sink.log, sink.anchor, &[]).is_err());
        }
    }
    #[test]
    fn independent_writers_serialize_without_loss() {
        let (_dir, sink) = setup();
        let threads: Vec<_> = (0..4)
            .map(|i| {
                let sink = sink.clone();
                std::thread::spawn(move || {
                    for j in 0..8 {
                        sink.append(serde_json::json!({"worker":i,"event":j}))
                            .unwrap();
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let lines = fs::read_to_string(&sink.log).unwrap();
        assert_eq!(lines.lines().count(), 32);
        for (i, line) in lines.lines().enumerate() {
            let row: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(row["audit_chain"]["sequence"], i + 1);
        }
    }
}
