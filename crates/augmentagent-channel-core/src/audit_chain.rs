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

// Bound the serialized representation, including a pending line's second
// layer of JSON escaping. Do not allocate a complete encoded anchor first.
struct BoundedAnchorWriter<'a,W:Write+?Sized> {
    writer:&'a mut W,
    remaining:usize,
}
impl<W:Write+?Sized> Write for BoundedAnchorWriter<'_,W> {
    fn write(&mut self,bytes:&[u8])->std::io::Result<usize> {
        if bytes.len()>self.remaining {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData,
                "audit anchor exceeds workflow budget"));
        }
        let written=self.writer.write(bytes)?;
        self.remaining-=written;
        Ok(written)
    }
    fn flush(&mut self)->std::io::Result<()> {self.writer.flush()}
}
#[derive(Debug)]
pub(crate) struct GenerationSealed;
impl std::fmt::Display for GenerationSealed {
    fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result {write!(f,"audit generation is sealed")}
}
impl std::error::Error for GenerationSealed {}
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
    /// Writer registration and sealing share this cross-process ingress gate.
    /// The callback must persist its ticket before releasing the generation lock.
    pub(crate) fn with_open_generation<T>(&self,register:impl FnOnce()->Result<T>)->Result<T> {
        let _lock=self.lock_generation()?;
        if self.anchor.exists() {
            let file=private_open(&self.anchor,false)?;
            ensure!(file.metadata()?.len()<=LIMIT,"anchor exceeds budget");
            let anchor:Anchor=serde_json::from_reader(file)?;
            ensure!(anchor.version==1,"unsupported anchor");
            if anchor.sealed.is_some(){return Err(GenerationSealed.into());}
        }
        register()
    }
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
    /// Writers and sealers serialize on the log, the generation's stable inode.
    /// The anchor cannot hold the lock because every save replaces it by
    /// rename, and a sidecar lock file would leave a mutable non-evidence file
    /// in the directory that must contain the archived anchor alone.
    fn lock_generation(&self) -> Result<File> {
        let lock = private_open(&self.log, true)?;
        #[cfg(unix)]
        ensure!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } == 0,
            "audit lock failed"
        );
        Ok(lock)
    }
    fn save(&self, anchor: &Anchor) -> Result<()> {
        let parent = self.anchor.parent().unwrap();
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer(BoundedAnchorWriter {writer:temp.as_file_mut(),remaining:LIMIT as usize}, anchor)?;
        temp.as_file().sync_all()?;
        temp.persist(&self.anchor).map_err(|e| e.error)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    }
    /// Appends under a cross-process lock; a durable pending intent permits
    /// replay after a daemon crash without accepting loss of committed rows.
    pub fn append(&self, record: serde_json::Value) -> Result<()> {
        let _lock = self.lock_generation()?;
        self.append_locked(record)
    }
    /// Validate without replaying uncertain records, then atomically prevent all
    /// future appends. The caller validates kernel quiescence while holding the
    /// same lock as writers. A retry returns the identical immutable proof.
    pub fn seal_for_recovery<F>(&self, validate: F) -> Result<serde_json::Value>
    where F: FnOnce(&[serde_json::Value]) -> Result<serde_json::Value> {
        ensure!(self.anchor.exists() && self.log.exists(), "recovery requires existing audit evidence");
        let _lock = self.lock_generation()?;
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
    fn pending_anchor_expansion_refuses_before_committed_evidence_changes() {
        let (_dir, sink) = setup();
        sink.append(serde_json::json!({"kind":"original"})).unwrap();
        let log_before=fs::read(&sink.log).unwrap();
        let anchor_before=fs::read(&sink.anchor).unwrap();
        // The log line fits 64 MiB, but embedding its escaping in pending
        // makes the recovery anchor exceed the reader's existing 64 MiB cap.
        let record=serde_json::json!({"payload":"\\".repeat(22*1024*1024)});
        assert!(serde_json::to_vec(&record).unwrap().len() < LIMIT as usize);
        let result=sink.append(record);
        assert!(result.is_err(),"accepted an append whose pending anchor cannot be recovered");
        assert_eq!(fs::read(&sink.log).unwrap(),log_before);
        assert_eq!(fs::read(&sink.anchor).unwrap(),anchor_before);
        let entries:Vec<_>=fs::read_dir(sink.anchor.parent().unwrap()).unwrap()
            .map(|entry|entry.unwrap().path()).collect();
        assert_eq!(entries,vec![sink.anchor.clone()],
            "oversized pending serialization left non-anchor files beside the anchor");
        sink.append(serde_json::json!({"kind":"after-refusal"})).unwrap();
    }

    #[test]
    fn recovery_seal_refuses_oversized_validation_before_replacing_anchor() {
        let (_dir, sink) = setup();
        sink.append(serde_json::json!({"kind":"original"})).unwrap();
        let log_before=fs::read(&sink.log).unwrap();
        let anchor_before=fs::read(&sink.anchor).unwrap();
        let result=sink.seal_for_recovery(|_|Ok(serde_json::json!({"validation":"p".repeat(LIMIT as usize)})));
        assert!(result.is_err(),"saved a sealed anchor its own reader refuses");
        assert_eq!(fs::read(&sink.log).unwrap(),log_before);
        assert_eq!(fs::read(&sink.anchor).unwrap(),anchor_before);
        assert_eq!(fs::read_dir(sink.anchor.parent().unwrap()).unwrap().count(),1);
        assert!(sink.seal_for_recovery(|_|Ok(serde_json::json!({"closed":true}))).is_ok());
    }

    #[test]
    fn anchor_encoded_limit_is_inclusive_and_overflow_preserves_original_object() {
        let (_dir,sink)=setup();
        let mut anchor=Anchor {version:1,pending:Some(String::new()),..Default::default()};
        let overhead=serde_json::to_vec(&anchor).unwrap().len();
        anchor.pending=Some("p".repeat(LIMIT as usize-overhead));
        sink.save(&anchor).unwrap();
        let before=fs::metadata(&sink.anchor).unwrap();
        assert_eq!(before.len(),LIMIT);
        let decoded:Anchor=serde_json::from_reader(std::io::BufReader::new(private_open(&sink.anchor,false).unwrap())).unwrap();
        assert_eq!(decoded.pending.as_ref().unwrap().len(),LIMIT as usize-overhead);
        anchor.pending.as_mut().unwrap().push('p');
        assert!(sink.save(&anchor).is_err(),"accepted LIMIT+1 encoded bytes");
        let after=fs::metadata(&sink.anchor).unwrap();
        assert_eq!((after.dev(),after.ino(),after.len()),(before.dev(),before.ino(),LIMIT));
        let current:Anchor=serde_json::from_reader(std::io::BufReader::new(private_open(&sink.anchor,false).unwrap())).unwrap();
        assert_eq!(current.pending,decoded.pending);
        assert_eq!(fs::read_dir(sink.anchor.parent().unwrap()).unwrap().count(),1);
    }

    #[test]
    fn anchor_stream_counts_actual_short_writes_and_propagates_io_failure() {
        struct ScriptedWriter {bytes:Vec<u8>,zero:bool,fail:bool}
        impl Write for ScriptedWriter {
            fn write(&mut self,data:&[u8])->std::io::Result<usize> {
                if self.fail {return Err(std::io::Error::other("scripted write failure"))}
                if self.zero {return Ok(0)}
                let count=data.len().min(2);self.bytes.extend_from_slice(&data[..count]);Ok(count)
            }
            fn flush(&mut self)->std::io::Result<()> {Ok(())}
        }
        let mut inner=ScriptedWriter {bytes:Vec::new(),zero:false,fail:false};
        {
            let mut writer=BoundedAnchorWriter {writer:&mut inner,remaining:5};
            writer.write_all(b"abc").unwrap();
            assert_eq!(writer.remaining,2);
            writer.write_all(b"de").unwrap();
            assert_eq!(writer.remaining,0);
            assert_eq!(writer.write_all(b"f").unwrap_err().kind(),std::io::ErrorKind::InvalidData);
        }
        assert_eq!(inner.bytes,b"abcde");
        for (zero,fail) in [(true,false),(false,true)] {
            let mut inner=ScriptedWriter {bytes:Vec::new(),zero,fail};
            let mut writer=BoundedAnchorWriter {writer:&mut inner,remaining:5};
            let error=writer.write_all(b"abc").unwrap_err();
            assert_eq!(error.kind(),if zero {std::io::ErrorKind::WriteZero}else{std::io::ErrorKind::Other});
            assert_eq!(writer.remaining,5);
            assert!(inner.bytes.is_empty());
        }
        let mut untouched=Vec::new();
        assert!(BoundedAnchorWriter {writer:&mut untouched,remaining:2}.write_all(b"abc").is_err());
        assert!(untouched.is_empty(),"single oversized chunk must refuse before its write");
    }

    #[test]
    fn oversized_append_does_not_rollback_prior_pending_recovery() {
        let (_dir,sink)=setup();
        sink.append(serde_json::json!({"kind":"original"})).unwrap();
        let mut anchor:Anchor=serde_json::from_slice(&fs::read(&sink.anchor).unwrap()).unwrap();
        anchor.pending=Some("{\"recovered\":true}\n".into());
        sink.save(&anchor).unwrap();
        private_open(&sink.log,true).unwrap().write_all(b"{\"recover").unwrap();
        assert!(sink.append(serde_json::json!({"payload":"\\".repeat(22*1024*1024)})).is_err());
        let current:Anchor=serde_json::from_slice(&fs::read(&sink.anchor).unwrap()).unwrap();
        assert!(current.pending.is_none());
        let log=fs::read_to_string(&sink.log).unwrap();
        assert_eq!(log.lines().count(),2);
        assert_eq!(log.matches("recovered").count(),1);
        for line in log.lines() {
            assert!(serde_json::from_str::<serde_json::Value>(line).unwrap().get("payload").is_none());
        }
        sink.append(serde_json::json!({"kind":"after-refusal"})).unwrap();
        assert_eq!(fs::read_to_string(&sink.log).unwrap().lines().count(),3);
    }

    #[test]
    fn already_sealed_retry_keeps_qualified_original_proof_without_new_serialization() {
        let (_dir,sink)=setup();
        sink.append(serde_json::json!({"kind":"original"})).unwrap();
        let proof=sink.seal_for_recovery(|_|Ok(serde_json::json!({"closed":true}))).unwrap();
        let before=fs::read(&sink.anchor).unwrap();
        let mut qualified=false;
        let repeated=sink.seal_for_recovery(|_| {
            qualified=true;
            Ok(serde_json::json!({"unused":"p".repeat(LIMIT as usize+1)}))
        }).unwrap();
        assert!(qualified);
        assert_eq!(repeated,proof);
        assert_eq!(fs::read(&sink.anchor).unwrap(),before);
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
