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
/// Reference to one fully committed record, captured under its append lock.
/// The record digest covers its exact JSON line without the trailing newline;
/// the committed digest covers the complete log prefix through that newline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppendReceipt {
    pub version: u32,
    pub sequence: u64,
    pub record_sha256: String,
    pub committed_audit_sha256: String,
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
fn verify_existing_identity(path: &Path, file: &File) -> Result<()> {
    let held = file.metadata()?;
    let current = fs::symlink_metadata(path)?;
    ensure!(held.is_file() && current.is_file() && !current.file_type().is_symlink(), "audit evidence is not a regular file");
    #[cfg(unix)]
    ensure!(
        held.uid() == unsafe { libc::geteuid() } && held.mode() & 0o077 == 0
            && held.nlink() == 1 && held.dev() == current.dev() && held.ino() == current.ino(),
        "audit evidence is not private or its identity changed"
    );
    Ok(())
}
fn private_existing(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK);
    let file = options.open(path)?;
    verify_existing_identity(path, &file)?;
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
fn verify_recovery_seal(anchor:&Anchor,proof:&serde_json::Value)->Result<()> {
    ensure!(proof["validation"].get("handoff_context").is_none(),"audit generation is sealed for a native handoff");
    ensure!(proof["version"].as_u64()==Some(1)
        && proof["committed_sha256"].as_str()==Some(anchor.hash.as_str())
        && proof["committed_bytes"].as_u64()==Some(u64::try_from(anchor.bytes)?)
        && proof["sequence"].as_u64()==Some(anchor.sequence)
        && proof["interrupted_append"]==serde_json::to_value(&anchor.pending)?,
        "recovery seal does not describe its original committed authority");
    Ok(())
}

impl AnchoredAudit {
    /// Verify a historical append reference against this existing audit only.
    /// This lookup grants no current runtime or transfer authority.
    /// A busy writer returns an I/O WouldBlock error for a fenced retry.
    pub fn read_receipt(&self, receipt: &AppendReceipt) -> Result<serde_json::Value> {
        let hash = |value: &str| value.len() == 64
            && value.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'));
        ensure!(receipt.version == 1 && receipt.sequence > 0
            && hash(&receipt.record_sha256) && hash(&receipt.committed_audit_sha256),
            "invalid audit append receipt");
        let lock_path = self.anchor.with_extension("lock");
        ensure!(lock_path != self.anchor && lock_path != self.log, "audit lock path collision");
        // Open original evidence only. In particular, never recreate a missing
        // lock inode and accidentally coordinate with a different generation.
        let lock = private_existing(&lock_path)?;
        #[cfg(unix)]
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        verify_existing_identity(&lock_path, &lock)?;
        let anchor_file = private_existing(&self.anchor)?;
        let log_file = private_existing(&self.log)?;
        ensure!(anchor_file.metadata()?.len() <= LIMIT && log_file.metadata()?.len() <= LIMIT,
            "audit evidence exceeds budget");
        let mut anchor_bytes = Vec::new();
        (&anchor_file).take(LIMIT + 1).read_to_end(&mut anchor_bytes)?;
        ensure!(anchor_bytes.len() as u64 <= LIMIT, "anchor exceeds budget");
        let anchor: Anchor = serde_json::from_slice(&anchor_bytes)?;
        ensure!(anchor.version == 1, "audit has unsupported authority");
        if anchor.pending.is_some() {
            verify_recovery_seal(&anchor,anchor.sealed.as_ref().context("open pending audit has uncertain authority")?)?;
        }
        let mut bytes = Vec::new();
        (&log_file).take(LIMIT + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() as u64 <= LIMIT && bytes.len() >= anchor.bytes,
            "audit has truncated bytes");
        let tail=&bytes[anchor.bytes..];
        ensure!(match &anchor.pending {Some(pending)=>pending.as_bytes().starts_with(tail),None=>tail.is_empty()},
            "audit has unrecognized uncommitted bytes");
        // Historical lookup never repairs/replays the interrupted append. Even
        // a complete-looking pending line is outside this committed prefix.
        bytes.truncate(anchor.bytes);
        let committed_hash = if bytes.is_empty() { String::new() } else { digest(&bytes) };
        ensure!(committed_hash == anchor.hash, "committed audit modified");
        let mut offset = 0;
        let mut sequence = 0;
        let mut found = None;
        for line in bytes.split_inclusive(|byte| *byte == b'\n') {
            ensure!(line.ends_with(b"\n"), "committed audit record is incomplete");
            let end = offset + line.len();
            let raw = &line[..line.len()-1];
            if !raw.is_empty() {
                sequence += 1;
                let row: serde_json::Value = serde_json::from_slice(raw)?;
                ensure!(row.is_object(), "committed audit record is not an object");
                if sequence == receipt.sequence {
                    ensure!(digest(raw) == receipt.record_sha256
                        && digest(&bytes[..end]) == receipt.committed_audit_sha256,
                        "audit receipt does not match its committed record");
                    let mut payload = row.clone();
                    let chain = payload.as_object_mut().unwrap().remove("audit_chain")
                        .context("referenced record lacks append metadata")?;
                    let previous = if offset == 0 { String::new() } else { digest(&bytes[..offset]) };
                    ensure!(chain.as_object().is_some_and(|object| object.len() == 4)
                        && chain["version"].as_u64() == Some(1)
                        && chain["sequence"].as_u64() == Some(sequence)
                        && chain["previous"].as_str() == Some(previous.as_str())
                        && chain["payload_sha256"].as_str() == Some(digest(&serde_json::to_vec(&payload)?).as_str()),
                        "referenced record has invalid append metadata");
                    found = Some(row);
                }
            }
            offset = end;
        }
        ensure!(sequence == anchor.sequence, "committed audit sequence mismatch");
        verify_existing_identity(&lock_path, &lock)?;
        verify_existing_identity(&self.anchor, &anchor_file)?;
        verify_existing_identity(&self.log, &log_file)?;
        found.context("audit receipt record is missing")
    }
    /// Writer registration and sealing share this cross-process ingress gate.
    /// The callback must persist its ticket before releasing the anchor lock.
    pub(crate) fn with_open_generation<T>(&self,register:impl FnOnce()->Result<T>)->Result<T> {
        let lock_path=self.anchor.with_extension("lock");
        ensure!(lock_path!=self.anchor && lock_path!=self.log,"audit lock path collision");
        let lock=private_open(&lock_path,false)?;
        #[cfg(unix)]
        ensure!(unsafe{libc::flock(lock.as_raw_fd(),libc::LOCK_EX)}==0,"audit lock failed");
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
        self.append_receipted(record).map(|_| ())
    }
    pub fn append_receipted(&self, record: serde_json::Value) -> Result<AppendReceipt> {
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
        let (_lock,mut anchor,rows,_)=self.locked_snapshot()?;
        if let Some(proof)=&anchor.sealed {verify_recovery_seal(&anchor,proof)?;}
        let validation = validate(&rows)?;
        if let Some(proof) = anchor.sealed { return Ok(proof); }
        let proof = serde_json::json!({"version":1,"committed_sha256":anchor.hash,
            "committed_bytes":anchor.bytes,"sequence":anchor.sequence,
            "interrupted_append":anchor.pending,"validation":validation});
        anchor.sealed = Some(proof.clone());
        self.save(&anchor)?;
        Ok(proof)
    }

    /// Healthy handoff replay reads the immutable seal before inspecting any
    /// later rejected writers. It cannot seal a pending or damaged append.
    pub(crate) fn seal_for_handoff<F>(&self,context:&serde_json::Value,validate:F)->Result<serde_json::Value>
    where F:FnOnce(&[serde_json::Value])->Result<serde_json::Value> {
        let (_lock,mut anchor,rows,clean)=self.locked_snapshot()?;
        ensure!(clean,"healthy handoff requires a fully committed audit");
        if let Some(proof)=&anchor.sealed {
            ensure!(proof["version"]==1 && proof["validation"]["handoff_context"]==*context
                && proof["committed_sha256"]==anchor.hash && proof["committed_bytes"]==anchor.bytes
                && proof["sequence"]==anchor.sequence,"audit seal belongs to another handoff");
            return Ok(proof.clone());
        }
        let observation=validate(&rows)?;
        let proof=serde_json::json!({"version":1,"committed_sha256":anchor.hash,
            "committed_bytes":anchor.bytes,"sequence":anchor.sequence,
            "validation":{"handoff_context":context,"observation":observation}});
        anchor.sealed=Some(proof.clone());self.save(&anchor)?;Ok(proof)
    }

    pub(crate) fn inspect_open<F>(&self,validate:F)->Result<()> where F:FnOnce(&[serde_json::Value])->Result<()> {
        let (_lock,anchor,rows,clean)=self.locked_snapshot()?;
        ensure!(clean && anchor.sealed.is_none(),"audit generation is not healthy and open");validate(&rows)
    }
    pub(crate) fn log_path(&self)->&Path {&self.log}
    pub(crate) fn anchor_path(&self)->&Path {&self.anchor}

    fn locked_snapshot(&self)->Result<(File,Anchor,Vec<serde_json::Value>,bool)> {
        let lock = private_open(&self.anchor.with_extension("lock"), false)?;
        #[cfg(unix)]
        ensure!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } == 0, "audit lock failed");
        ensure!(self.anchor.exists() && self.log.exists(), "recovery requires existing audit evidence");
        let file = private_open(&self.anchor, false)?;
        ensure!(file.metadata()?.len() <= LIMIT, "anchor exceeds budget");
        let anchor: Anchor = serde_json::from_reader(file)?;
        ensure!(anchor.version == 1, "unsupported anchor");
        let mut file = private_open(&self.log, false)?;
        ensure!(file.metadata()?.len() <= LIMIT, "audit exceeds budget");
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        ensure!(bytes.len() >= anchor.bytes, "committed audit truncated");
        let committed = &bytes[..anchor.bytes];
        let actual = if committed.is_empty() { String::new() } else { digest(committed) };
        ensure!(actual == anchor.hash, "committed audit modified");
        ensure!(anchor.sequence == committed.split(|byte| *byte==b'\n').filter(|row| !row.is_empty()).count() as u64, "committed audit sequence mismatch");
        let tail = &bytes[anchor.bytes..];
        ensure!(match &anchor.pending { Some(p) => p.as_bytes().starts_with(tail), None => tail.is_empty() }, "unrecognized audit tail");
        let rows = committed.split(|b| *b == b'\n').filter(|r| !r.is_empty())
            .map(serde_json::from_slice).collect::<std::result::Result<Vec<serde_json::Value>, _>>()?;
        let clean=anchor.pending.is_none() && tail.is_empty();
        Ok((lock,anchor,rows,clean))
    }
    fn append_locked(&self, mut record: serde_json::Value) -> Result<AppendReceipt> {
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
        ensure!(anchor.sequence == committed.split(|byte| *byte==b'\n').filter(|row| !row.is_empty()).count() as u64, "committed audit sequence mismatch");
        if let Some(pending) = anchor.pending.take() {
            ensure!(pending.ends_with('\n') && !pending[..pending.len()-1].contains('\n'), "pending audit record framing is invalid");
            ensure!(serde_json::from_str::<serde_json::Value>(&pending)?.is_object(), "pending audit record must be an object");
            ensure!(anchor.bytes.checked_add(pending.len()).is_some_and(|bytes| bytes<=LIMIT as usize), "pending audit exceeds workflow budget");
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
        let record_sha256 = digest(line.as_bytes());
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
        self.save(&anchor)?;
        Ok(AppendReceipt { version:1, sequence:anchor.sequence, record_sha256, committed_audit_sha256:anchor.hash })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn receipt_lookup_returns_its_exact_record_after_later_appends_and_sealing() {
        let (_directory, sink) = setup();
        let first = sink.append_receipted(serde_json::json!({"kind":"launch","request":"original"})).unwrap();
        sink.append(serde_json::json!({"kind":"later","request":"other"})).unwrap();
        let before_log = fs::read(&sink.log).unwrap();
        let before_anchor = fs::read(&sink.anchor).unwrap();
        let row = sink.read_receipt(&first).unwrap();
        assert_eq!(row["kind"], "launch");
        assert_eq!(row["request"], "original");
        assert_eq!(fs::read(&sink.log).unwrap(), before_log);
        assert_eq!(fs::read(&sink.anchor).unwrap(), before_anchor);
        sink.seal_for_recovery(|_| Ok(serde_json::json!({"verified":"fixture"}))).unwrap();
        let sealed = fs::read(&sink.anchor).unwrap();
        assert_eq!(sink.read_receipt(&first).unwrap(), row);
        assert_eq!(fs::read(&sink.anchor).unwrap(), sealed);
    }
    #[test]
    fn receipt_lookup_rejects_forged_or_mismatched_references() {
        let (_directory, sink) = setup();
        let receipt = sink.append_receipted(serde_json::json!({"kind":"launch"})).unwrap();
        for mode in ["version", "zero", "future", "record", "prefix", "uppercase", "short"] {
            let mut wrong = receipt.clone();
            match mode {
                "version" => wrong.version = 2,
                "zero" => wrong.sequence = 0,
                "future" => wrong.sequence += 1,
                "record" => wrong.record_sha256 = "0".repeat(64),
                "prefix" => wrong.committed_audit_sha256 = "0".repeat(64),
                "uppercase" => wrong.record_sha256 = "A".repeat(64),
                "short" => { wrong.record_sha256.pop(); },
                _ => unreachable!(),
            }
            assert!(sink.read_receipt(&wrong).is_err(), "{mode}");
        }
        let (_other_directory, other) = setup();
        other.append(serde_json::json!({"kind":"different"})).unwrap();
        assert!(other.read_receipt(&receipt).is_err());
        assert_eq!(sink.read_receipt(&receipt).unwrap()["kind"], "launch");
    }
    #[test]
    fn receipt_lookup_does_not_recreate_missing_evidence() {
        for missing in ["log", "anchor", "lock"] {
            let (_directory, sink) = setup();
            let receipt = sink.append_receipted(serde_json::json!({"kind":"launch"})).unwrap();
            let path = match missing { "log" => sink.log.clone(), "anchor" => sink.anchor.clone(), _ => sink.anchor.with_extension("lock") };
            fs::remove_file(&path).unwrap();
            assert!(sink.read_receipt(&receipt).is_err(), "{missing}");
            assert!(!path.exists(), "lookup recreated {missing}");
        }
    }
    #[test]
    fn receipt_lookup_refuses_corruption_or_pending_data_without_repair() {
        for mode in ["corrupt", "counter", "pending", "tail"] {
            let (_directory, sink) = setup();
            let receipt = sink.append_receipted(serde_json::json!({"kind":"launch"})).unwrap();
            match mode {
                "corrupt" => { let mut bytes=fs::read(&sink.log).unwrap(); bytes[2]^=1; fs::write(&sink.log,bytes).unwrap(); },
                "counter" | "pending" => {
                    let mut anchor: Anchor=serde_json::from_slice(&fs::read(&sink.anchor).unwrap()).unwrap();
                    if mode=="counter" {anchor.sequence+=1;} else {anchor.pending=Some("{\"next\":true}\n".into());}
                    sink.save(&anchor).unwrap();
                },
                "tail" => private_open(&sink.log,true).unwrap().write_all(b"uncertain").unwrap(),
                _ => unreachable!(),
            }
            let log=fs::read(&sink.log).unwrap(); let anchor=fs::read(&sink.anchor).unwrap();
            assert!(sink.read_receipt(&receipt).is_err(), "{mode}");
            assert_eq!(fs::read(&sink.log).unwrap(),log); assert_eq!(fs::read(&sink.anchor).unwrap(),anchor);
        }
    }
    #[cfg(unix)]
    #[test]
    fn receipt_lookup_refuses_links_and_nonprivate_evidence() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        for name in ["log", "anchor", "lock"] {
            for mode in ["symlink", "hardlink", "public"] {
                let (directory, sink) = setup();
                let receipt = sink.append_receipted(serde_json::json!({"kind":"launch"})).unwrap();
                let path = match name { "log"=>sink.log.clone(), "anchor"=>sink.anchor.clone(), _=>sink.anchor.with_extension("lock") };
                let before = fs::read(&path).unwrap();
                let alias = directory.path().join("alias");
                match mode {
                    "symlink" => { fs::rename(&path,&alias).unwrap(); symlink(&alias,&path).unwrap(); },
                    "hardlink" => fs::hard_link(&path,&alias).unwrap(),
                    "public" => fs::set_permissions(&path,fs::Permissions::from_mode(0o644)).unwrap(),
                    _ => unreachable!(),
                }
                assert!(sink.read_receipt(&receipt).is_err(), "{name}/{mode}");
                assert_eq!(fs::read(&path).unwrap(),before);
                if mode=="symlink" {assert!(fs::symlink_metadata(&path).unwrap().file_type().is_symlink());}
            }
        }
    }
    #[test]
    fn receipt_lookup_reads_committed_prefix_of_recovery_sealed_pending_audit() {
        let pending="{\"kind\":\"uncommitted\",\"text\":\"é\"}\n";
        for length in [0,7,pending.find('é').unwrap()+1,pending.len()] {
            let (_directory,sink)=setup();
            let receipt=sink.append_receipted(serde_json::json!({"kind":"committed"})).unwrap();
            let mut anchor:Anchor=serde_json::from_slice(&fs::read(&sink.anchor).unwrap()).unwrap();
            anchor.pending=Some(pending.into());sink.save(&anchor).unwrap();
            private_open(&sink.log,true).unwrap().write_all(&pending.as_bytes()[..length]).unwrap();
            assert!(sink.read_receipt(&receipt).is_err(),"open pending authority became a receipt source");
            sink.seal_for_recovery(|_|Ok(serde_json::json!({"fixture":"metadata-only"}))).unwrap();
            let log=fs::read(&sink.log).unwrap();let anchor=fs::read(&sink.anchor).unwrap();
            assert_eq!(sink.read_receipt(&receipt).unwrap()["kind"],"committed");
            let mut future=receipt.clone();future.sequence+=1;
            future.record_sha256=digest(&pending.as_bytes()[..pending.len()-1]);
            future.committed_audit_sha256=digest(&log);
            assert!(sink.read_receipt(&future).is_err(),"uncommitted pending row became a committed receipt");
            assert_eq!(fs::read(&sink.log).unwrap(),log,"historical lookup replayed pending bytes");
            assert_eq!(fs::read(&sink.anchor).unwrap(),anchor,"historical lookup rewrote recovery authority");
            assert!(sink.append(serde_json::json!({"kind":"late"})).is_err());
        }
    }
    #[test]
    fn recovery_sealed_receipt_refuses_changed_seal_pending_and_tail_without_repair() {
        for fault in ["tail","seal-hash","seal-pending","pending","sequence","handoff-context","truncated"] {
            let (_directory,sink)=setup();
            let receipt=sink.append_receipted(serde_json::json!({"kind":"committed"})).unwrap();
            let mut anchor:Anchor=serde_json::from_slice(&fs::read(&sink.anchor).unwrap()).unwrap();
            anchor.pending=Some("{\"kind\":\"pending\"}\n".into());sink.save(&anchor).unwrap();
            private_open(&sink.log,true).unwrap().write_all(b"{\"kind\":").unwrap();
            sink.seal_for_recovery(|_|Ok(serde_json::json!({"fixture":"metadata-only"}))).unwrap();
            let mut anchor:Anchor=serde_json::from_slice(&fs::read(&sink.anchor).unwrap()).unwrap();
            match fault {
                "tail"=>private_open(&sink.log,true).unwrap().write_all(b"invalid").unwrap(),
                "seal-hash"=>anchor.sealed.as_mut().unwrap()["committed_sha256"]="f".repeat(64).into(),
                "seal-pending"=>anchor.sealed.as_mut().unwrap()["interrupted_append"]="another".into(),
                "pending"=>anchor.pending=Some("{\"kind\":\"other\"}\n".into()),
                "sequence"=>{anchor.sequence+=1;anchor.sealed.as_mut().unwrap()["sequence"]=anchor.sequence.into();},
                "handoff-context"=>anchor.sealed.as_mut().unwrap()["validation"]["handoff_context"]=serde_json::json!({"request":"other"}),
                "truncated"=>{let bytes=fs::read(&sink.log).unwrap();fs::write(&sink.log,&bytes[..anchor.bytes-1]).unwrap();},
                _=>unreachable!(),
            }
            sink.save(&anchor).unwrap();let before=(fs::read(&sink.log).unwrap(),fs::read(&sink.anchor).unwrap());
            assert!(sink.read_receipt(&receipt).is_err(),"accepted {fault}");
            assert_eq!((fs::read(&sink.log).unwrap(),fs::read(&sink.anchor).unwrap()),before,"lookup repaired {fault}");
        }
    }
    #[test]
    fn recovery_seal_rejects_handoff_before_invoking_recovery_validator() {
        let (_directory,sink)=setup();
        let receipt=sink.append_receipted(serde_json::json!({"kind":"committed"})).unwrap();
        sink.seal_for_handoff(&serde_json::json!({"request":"fixture"}),|_|Ok(serde_json::json!({"fixture":"handoff"}))).unwrap();
        let anchor=fs::read(&sink.anchor).unwrap();let calls=std::cell::Cell::new(0);
        let result=sink.seal_for_recovery(|_|{calls.set(calls.get()+1);Ok(serde_json::json!({"fixture":"recovery"}))});
        assert!(result.is_err(),"a handoff seal was returned as a recovery proof");
        assert_eq!(calls.get(),0,"recovery validation ran against a handoff-sealed namespace");
        assert_eq!(fs::read(&sink.anchor).unwrap(),anchor);
        assert_eq!(sink.read_receipt(&receipt).unwrap()["kind"],"committed");
    }
    #[test]
    fn receipt_lookup_serializes_with_concurrent_appends() {
        let (_directory, sink) = setup();
        let receipt = sink.append_receipted(serde_json::json!({"kind":"launch","request":"first"})).unwrap();
        let writer = sink.clone();
        let task = std::thread::spawn(move || {
            for index in 0..12 {writer.append(serde_json::json!({"kind":"other","index":index})).unwrap();}
        });
        let read = || {
            let deadline = std::time::Instant::now()+std::time::Duration::from_secs(5);
            loop {
                match sink.read_receipt(&receipt) {
                    Ok(row) => break row,
                    Err(error) if error.downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind()==std::io::ErrorKind::WouldBlock) => {
                        assert!(std::time::Instant::now()<deadline,"writer never settled");
                        std::thread::sleep(std::time::Duration::from_millis(2));
                    },
                    Err(error) => panic!("{error:#}"),
                }
            }
        };
        for _ in 0..12 {
            let row = read();
            assert_eq!(row["kind"],"launch"); assert_eq!(row["request"],"first");
        }
        task.join().unwrap();
        assert_eq!(sink.read_receipt(&receipt).unwrap()["audit_chain"]["sequence"],1);
    }
    #[cfg(unix)]
    #[test]
    fn receipt_lookup_busy_writer_is_retryable_without_waiting_for_it() {
        let (_directory, sink) = setup();
        let receipt = sink.append_receipted(serde_json::json!({"kind":"launch"})).unwrap();
        let lock = private_existing(&sink.anchor.with_extension("lock")).unwrap();
        assert_eq!(unsafe {libc::flock(lock.as_raw_fd(),libc::LOCK_EX)},0);
        let (send, receive) = std::sync::mpsc::channel();
        let reader = sink.clone(); let request = receipt.clone();
        let task = std::thread::spawn(move || {send.send(reader.read_receipt(&request)).unwrap();});
        let result = receive.recv_timeout(std::time::Duration::from_secs(2));
        // Release even on failure so this regression never hangs the suite.
        drop(lock); task.join().unwrap();
        let error = result.expect("lookup waited for a held writer lock").unwrap_err();
        assert_eq!(error.downcast_ref::<std::io::Error>().unwrap().kind(),std::io::ErrorKind::WouldBlock);
        assert_eq!(sink.read_receipt(&receipt).unwrap()["kind"],"launch");
    }
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
    fn append_receipt_keeps_each_concurrent_writer_bound_to_its_own_record() {
        let (_directory, sink) = setup();
        let workers: Vec<_> = (0..8).map(|worker| {
            let sink = sink.clone();
            std::thread::spawn(move || (worker, sink.append_receipted(serde_json::json!({"actor":"controller","kind":"native_launch_intent","worker":worker})).unwrap()))
        }).collect();
        let receipts: Vec<_> = workers.into_iter().map(|worker| worker.join().unwrap()).collect();
        let bytes = fs::read(&sink.log).unwrap();
        let lines: Vec<_> = bytes.split_inclusive(|byte| *byte==b'\n').collect();
        assert_eq!(lines.len(), 8);
        for (worker, receipt) in receipts {
            assert_eq!(receipt.version, 1);
            let index = usize::try_from(receipt.sequence-1).unwrap();
            let line = lines[index];
            let row: serde_json::Value = serde_json::from_slice(line).unwrap();
            assert_eq!(row["worker"], worker);
            assert_eq!(row["audit_chain"]["sequence"], receipt.sequence);
            assert_eq!(receipt.record_sha256, digest(&line[..line.len()-1]));
            assert_eq!(receipt.committed_audit_sha256, digest(&lines[..=index].concat()));
        }
    }
    #[test]
    fn append_receipt_identifies_the_new_record_after_recovering_a_pending_append() {
        let (_directory, sink) = setup();
        sink.append(serde_json::json!({"kind":"first"})).unwrap();
        let mut anchor: Anchor = serde_json::from_slice(&fs::read(&sink.anchor).unwrap()).unwrap();
        anchor.pending = Some("{\"kind\":\"recovered\"}\n".into());
        sink.save(&anchor).unwrap();
        private_open(&sink.log, true).unwrap().write_all(b"{\"kind\":").unwrap();
        let receipt = sink.append_receipted(serde_json::json!({"kind":"new-launch"})).unwrap();
        assert_eq!(receipt.sequence, 3);
        let bytes = fs::read(&sink.log).unwrap();
        let lines: Vec<_> = bytes.split_inclusive(|byte| *byte==b'\n').collect();
        assert_eq!(lines.len(), 3);
        let row: serde_json::Value = serde_json::from_slice(lines[2]).unwrap();
        assert_eq!(row["kind"], "new-launch");
        assert_eq!(receipt.record_sha256, digest(&lines[2][..lines[2].len()-1]));
        assert_eq!(receipt.committed_audit_sha256, digest(&bytes));
    }
    #[test]
    fn append_receipt_refuses_sealed_or_corrupt_authority_without_rewriting_it() {
        for mode in ["corrupt", "sealed", "counter"] {
            let (_directory, sink) = setup();
            sink.append(serde_json::json!({"kind":"first"})).unwrap();
            if mode == "sealed" {
                sink.seal_for_recovery(|_| Ok(serde_json::json!({}))).unwrap();
            } else if mode == "counter" {
                let mut anchor: Anchor = serde_json::from_slice(&fs::read(&sink.anchor).unwrap()).unwrap();
                anchor.sequence += 1;
                sink.save(&anchor).unwrap();
            } else {
                let mut bytes = fs::read(&sink.log).unwrap();
                bytes[2] ^= 1;
                fs::write(&sink.log, bytes).unwrap();
            }
            let before = fs::read(&sink.log).unwrap();
            assert!(sink.append_receipted(serde_json::json!({"kind":"late"})).is_err());
            assert_eq!(fs::read(&sink.log).unwrap(), before);
        }
    }
    #[test]
    fn append_receipt_refuses_malformed_pending_records_before_recovering_bytes() {
        for pending in ["\n", "[]\n", "{}\n{}\n"] {
            let (_directory, sink) = setup();
            sink.append(serde_json::json!({"kind":"first"})).unwrap();
            let mut anchor: Anchor = serde_json::from_slice(&fs::read(&sink.anchor).unwrap()).unwrap();
            anchor.pending = Some(pending.into());
            sink.save(&anchor).unwrap();
            let before = fs::read(&sink.log).unwrap();
            assert!(sink.append_receipted(serde_json::json!({"kind":"new"})).is_err());
            assert_eq!(fs::read(&sink.log).unwrap(), before);
        }
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
