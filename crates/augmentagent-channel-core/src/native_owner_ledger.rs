//! Private transactional authority. Callers still validate filesystem inventory,
//! audit seals and current workflow fencing before committing through this layer.
use super::ownership::TransferReason as R;
use super::ownership::{
    canonical_bytes, Authority, SealReference, TransferReceipt, TransferRequest,
};
use super::{RuntimeOwner, StateLock};
use anyhow::{ensure, Context, Result};
use augmentagent_store::rusqlite::{self, params, OptionalExtension};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, os::unix::fs::MetadataExt, path::Path};

const HISTORY_LIMIT: u64 = 128 * 1024 * 1024;
const SEALS_LIMIT: usize = 1024 * 1024;
const RECEIPT_LIMIT: usize = 256 * 1024;
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum State {
    Prepared,
    Completed(TransferReceipt),
    Aborted,
}
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Endpoint {
    pub audit_path: String,
    pub actual_audit_path: String,
    pub anchor_path: String,
}
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Claim {
    kind: String,
    key: String,
}
struct Record {
    request: TransferRequest,
    claims: Vec<Claim>,
    reserved: u64,
    state: State,
}
pub(super) struct Ledger<'a> {
    root: &'a Path,
    _lock: &'a StateLock,
    db: rusqlite::Connection,
}
fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn encoded<T: serde::Serialize>(value: &T) -> Result<Vec<u8>> {
    canonical_bytes(&serde_json::to_value(value)?)
}
fn owner_key(owner: &RuntimeOwner) -> Result<String> {
    Ok(sha(&encoded(owner)?))
}
fn reservation(request: &[u8], claims: &[u8]) -> Result<u64> {
    (request.len() as u64)
        .checked_add(claims.len() as u64)
        .and_then(|bytes| bytes.checked_add((SEALS_LIMIT + RECEIPT_LIMIT) as u64))
        .and_then(|bytes| bytes.checked_mul(2))
        .and_then(|bytes| bytes.checked_add(1024 * 1024))
        .context(R::HistoryCapacity)
}
fn required_claims(request: &TransferRequest) -> Result<Vec<Claim>> {
    let mut claims = Vec::new();
    for authority in request
        .source_owners
        .iter()
        .chain(std::iter::once(&request.destination))
    {
        claims.push(Claim {
            kind: "owner".into(),
            key: owner_key(&authority.owner)?,
        });
        claims.push(Claim {
            kind: "audit".into(),
            key: authority.audit_path.clone(),
        });
    }
    for row in &request.inventory {
        claims.push(Claim {
            kind: "runtime".into(),
            key: row.runtime_relative_path.clone(),
        });
    }
    claims.sort();
    claims.dedup();
    Ok(claims)
}
fn claim_set(request: &TransferRequest, endpoints: &[Endpoint]) -> Result<Vec<Claim>> {
    let mut declarations = BTreeMap::new();
    let mut actual_files = BTreeMap::new();
    let mut anchors = BTreeMap::new();
    for endpoint in endpoints {
        ensure!(
            [&endpoint.actual_audit_path, &endpoint.anchor_path]
                .iter()
                .all(|path| Path::new(path).is_absolute() && !path.contains('\0')),
            R::InvalidRequest
        );
        let identity = (
            endpoint.actual_audit_path.clone(),
            endpoint.anchor_path.clone(),
        );
        if let Some(previous) = declarations.insert(endpoint.audit_path.clone(), identity.clone()) {
            ensure!(previous == identity, R::InvalidRequest);
        }
        if let Some(previous) = actual_files.insert(
            endpoint.actual_audit_path.clone(),
            endpoint.anchor_path.clone(),
        ) {
            ensure!(previous == endpoint.anchor_path, R::InvalidRequest);
        }
        if let Some(previous) = anchors.insert(
            endpoint.anchor_path.clone(),
            endpoint.actual_audit_path.clone(),
        ) {
            ensure!(previous == endpoint.actual_audit_path, R::InvalidRequest);
        }
    }
    let mut expected: Vec<_> = request
        .source_owners
        .iter()
        .chain(std::iter::once(&request.destination))
        .map(|owner| owner.audit_path.clone())
        .collect();
    expected.sort();
    expected.dedup();
    ensure!(declarations.keys().eq(expected.iter()), R::InvalidRequest);
    let mut claims = required_claims(request)?;
    for (audit_path, (actual_audit_path, anchor_path)) in declarations {
        let binding = Endpoint {
            audit_path,
            actual_audit_path,
            anchor_path,
        };
        claims.push(Claim {
            kind: "audit-binding".into(),
            key: String::from_utf8(encoded(&binding)?)?,
        });
    }
    claims.extend(actual_files.into_keys().map(|key| Claim {
        kind: "audit-file".into(),
        key,
    }));
    claims.extend(anchors.into_keys().map(|key| Claim {
        kind: "anchor".into(),
        key,
    }));
    claims.sort();
    claims.dedup();
    Ok(claims)
}

fn sources(db: &rusqlite::Connection, id: &str) -> Result<Vec<Authority>> {
    let mut query=db.prepare("SELECT workflow,generation,audit_path FROM native_transfer_source WHERE transfer_id=?1 ORDER BY generation,audit_path")?;
    let rows = query
        .query_map([id], |row| {
            Ok(Authority {
                owner: RuntimeOwner {
                    workflow_id: row.get(0)?,
                    lease_generation: row.get(1)?,
                },
                audit_path: row.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}
fn actual_claims(db: &rusqlite::Connection, id: &str) -> Result<Vec<Claim>> {
    let mut query = db.prepare(
        "SELECT kind,key FROM native_transfer_claim WHERE transfer_id=?1 ORDER BY kind,key",
    )?;
    let rows = query
        .query_map([id], |row| {
            Ok(Claim {
                kind: row.get(0)?,
                key: row.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}
fn load(db: &rusqlite::Connection, id: &str) -> Result<Option<Record>> {
    type Raw = (
        Vec<u8>,
        String,
        Vec<u8>,
        String,
        u64,
        String,
        Option<Vec<u8>>,
        Option<Vec<u8>>,
    );
    let raw:Option<Raw>=db.query_row("SELECT request_json,request_sha256,claims_json,claims_sha256,reserved_bytes,state,receipt_json,seals_json FROM native_transfer WHERE id=?1",[id],
        |row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?,row.get(6)?,row.get(7)?))).optional()?;
    let Some((bytes, digest, claim_bytes, claim_sha, reserved, state, receipt, seals)) = raw else {
        return Ok(None);
    };
    let request = TransferRequest::parse(&bytes).context(R::PreparedCorrupt)?;
    ensure!(
        request.transfer_id == id
            && request.request_digest()? == digest
            && encoded(&request)? == bytes
            && reserved == reservation(&bytes, &claim_bytes)?,
        R::PreparedCorrupt
    );
    let claims: Vec<Claim> = serde_json::from_slice(&claim_bytes)?;
    ensure!(
        sha(&claim_bytes) == claim_sha
            && encoded(&claims)? == claim_bytes
            && claims.windows(2).all(|pair| pair[0] < pair[1]),
        R::PreparedCorrupt
    );
    let bindings = claims
        .iter()
        .filter(|claim| claim.kind == "audit-binding")
        .map(|claim| serde_json::from_str::<Endpoint>(&claim.key))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    ensure!(
        claims == claim_set(&request, &bindings)?,
        R::PreparedCorrupt
    );
    ensure!(
        sources(db, id)? == request.source_owners,
        R::PreparedCorrupt
    );
    let active = actual_claims(db, id)?;
    let state = match state.as_str() {
        "prepared" => {
            ensure!(
                active == claims && receipt.is_none() && seals.is_none(),
                R::PreparedCorrupt
            );
            State::Prepared
        }
        "aborted" => {
            ensure!(
                active.is_empty() && receipt.is_none() && seals.is_none(),
                R::PreparedCorrupt
            );
            State::Aborted
        }
        "completed" => {
            ensure!(active.is_empty(), R::PreparedCorrupt);
            let bytes = receipt.context(R::PreparedCorrupt)?;
            ensure!(bytes.len() <= RECEIPT_LIMIT, R::PreparedCorrupt);
            let receipt: TransferReceipt = serde_json::from_slice(&bytes)?;
            receipt.verify(&request)?;
            let seals = seals.context(R::PreparedCorrupt)?;
            ensure!(
                seals.len() <= SEALS_LIMIT && seals == encoded(&receipt.source_seals)?,
                R::PreparedCorrupt
            );
            let mut query=db.prepare("SELECT ordinal,runtime FROM native_transfer_link WHERE transfer_id=?1 ORDER BY ordinal")?;
            let links = query
                .query_map([id], |row| {
                    Ok((row.get::<_, usize>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            ensure!(
                links
                    == request
                        .inventory
                        .iter()
                        .enumerate()
                        .map(|(index, row)| (index, row.runtime_relative_path.clone()))
                        .collect::<Vec<_>>(),
                R::PreparedCorrupt
            );
            let mut query=db.prepare("SELECT workflow,generation,audit_path FROM native_transfer_close WHERE transfer_id=?1 ORDER BY generation,audit_path")?;
            let closed = query
                .query_map([id], |row| {
                    Ok(Authority {
                        owner: RuntimeOwner {
                            workflow_id: row.get(0)?,
                            lease_generation: row.get(1)?,
                        },
                        audit_path: row.get(2)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            ensure!(closed == request.source_owners, R::PreparedCorrupt);
            State::Completed(receipt)
        }
        _ => anyhow::bail!(R::PreparedCorrupt),
    };
    Ok(Some(Record {
        request,
        claims,
        reserved,
        state,
    }))
}
fn transfer_closed(db: &rusqlite::Connection, owner: &RuntimeOwner) -> Result<bool> {
    let id: Option<String> = db
        .query_row(
            "SELECT transfer_id FROM native_transfer_close WHERE workflow=?1 AND generation=?2",
            params![owner.workflow_id, owner.lease_generation],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(id) = id {
        ensure!(
            matches!(
                load(db, &id)?.context(R::PreparedCorrupt)?.state,
                State::Completed(_)
            ),
            R::PreparedCorrupt
        );
        return Ok(true);
    }
    Ok(false)
}
fn closed(db: &rusqlite::Connection, owner: &RuntimeOwner) -> Result<bool> {
    Ok(transfer_closed(db, owner)?
        || db.query_row(
            "SELECT EXISTS(SELECT 1 FROM retired_owner WHERE workflow=?1 AND generation=?2)",
            params![owner.workflow_id, owner.lease_generation],
            |row| row.get(0),
        )?)
}
fn conflict(db: &rusqlite::Connection, owner: &RuntimeOwner) -> Result<bool> {
    let mut query=db.prepare("SELECT source.transfer_id FROM native_transfer_source source JOIN native_transfer pending ON pending.id=source.transfer_id WHERE pending.state='aborted' AND source.workflow=?1 AND source.generation=?2")?;
    let ids = query
        .query_map(params![owner.workflow_id, owner.lease_generation], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in &ids {
        ensure!(
            matches!(
                load(db, id)?.context(R::PreparedCorrupt)?.state,
                State::Aborted
            ),
            R::PreparedCorrupt
        );
    }
    Ok(!ids.is_empty())
}
fn growth(db: &rusqlite::Connection) -> Result<u64> {
    let mut query = db.prepare("SELECT id FROM native_transfer WHERE state='prepared'")?;
    let ids = query
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut total = 0u64;
    for id in ids {
        let record = load(db, &id)?.context(R::PreparedCorrupt)?;
        ensure!(matches!(record.state, State::Prepared), R::PreparedCorrupt);
        total = total
            .checked_add(record.reserved)
            .context(R::HistoryCapacity)?;
    }
    Ok(total)
}
pub(super) fn history_bytes(root: &Path) -> Result<u64> {
    let mut bytes = 0u64;
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let path = root.join(format!("runtime-retirement.sqlite{suffix}"));
        match path.symlink_metadata() {
            Ok(info) => {
                ensure!(
                    info.is_file()
                        && info.uid() == unsafe { libc::geteuid() }
                        && info.mode() & 0o077 == 0
                        && info.nlink() == 1,
                    R::PreparedCorrupt
                );
                bytes = bytes.checked_add(info.len()).context(R::HistoryCapacity)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(bytes)
}
impl<'a> Ledger<'a> {
    pub fn open(root: &'a Path, lock: &'a StateLock) -> Result<Self> {
        let held = lock.0.metadata()?;
        let current = root.join("state.lock").symlink_metadata()?;
        ensure!(
            held.dev() == current.dev() && held.ino() == current.ino(),
            R::LockBusy
        );
        let db = super::owner_store::open(root, lock)?;
        db.pragma_update(None, "foreign_keys", "ON")?;
        db.execute_batch("CREATE TABLE IF NOT EXISTS native_transfer_meta(id INTEGER PRIMARY KEY CHECK(id=1),version INTEGER NOT NULL);
            INSERT OR IGNORE INTO native_transfer_meta VALUES(1,1);
            CREATE TABLE IF NOT EXISTS native_transfer(id TEXT PRIMARY KEY,request_json BLOB NOT NULL,request_sha256 TEXT NOT NULL,claims_json BLOB NOT NULL,claims_sha256 TEXT NOT NULL,reserved_bytes INTEGER NOT NULL CHECK(reserved_bytes>0),state TEXT NOT NULL CHECK(state IN ('prepared','completed','aborted')),receipt_json BLOB,seals_json BLOB);
            CREATE TABLE IF NOT EXISTS native_transfer_claim(kind TEXT NOT NULL,key TEXT NOT NULL,transfer_id TEXT NOT NULL REFERENCES native_transfer(id),PRIMARY KEY(kind,key));
            CREATE TABLE IF NOT EXISTS native_transfer_source(transfer_id TEXT NOT NULL REFERENCES native_transfer(id),workflow TEXT NOT NULL,generation INTEGER NOT NULL,audit_path TEXT NOT NULL,PRIMARY KEY(transfer_id,workflow,generation));
            CREATE INDEX IF NOT EXISTS native_transfer_source_owner ON native_transfer_source(workflow,generation);
            CREATE TABLE IF NOT EXISTS native_transfer_close(workflow TEXT NOT NULL,generation INTEGER NOT NULL,audit_path TEXT NOT NULL,transfer_id TEXT NOT NULL REFERENCES native_transfer(id),PRIMARY KEY(workflow,generation));
            CREATE TABLE IF NOT EXISTS native_transfer_link(seq INTEGER PRIMARY KEY AUTOINCREMENT,transfer_id TEXT NOT NULL REFERENCES native_transfer(id),ordinal INTEGER NOT NULL,runtime TEXT NOT NULL,UNIQUE(transfer_id,ordinal),UNIQUE(transfer_id,runtime));
            CREATE INDEX IF NOT EXISTS native_transfer_link_runtime ON native_transfer_link(runtime,seq);")?;
        ensure!(
            db.query_row(
                "SELECT version FROM native_transfer_meta WHERE id=1",
                [],
                |row| row.get::<_, i64>(0)
            )? == 1,
            R::PreparedCorrupt
        );
        {
            let mut check = db.prepare("PRAGMA foreign_key_check")?;
            let mut rows = check.query([])?;
            ensure!(rows.next()?.is_none(), R::PreparedCorrupt);
        }
        growth(&db)?;
        super::owner_store::activate(root, lock, &db)?;
        std::fs::File::open(root)?.sync_all()?;
        Ok(Self {
            root,
            _lock: lock,
            db,
        })
    }
    pub fn state(&self, id: &str) -> Result<Option<State>> {
        Ok(load(&self.db, id)?.map(|record| record.state))
    }
    pub fn state_for(&self, request: &TransferRequest) -> Result<Option<State>> {
        let Some(prior) = load(&self.db, &request.transfer_id)? else {
            return Ok(None);
        };
        ensure!(prior.request == *request, R::AuthorityConflict);
        Ok(Some(prior.state))
    }
    pub fn reserved_bytes(&self) -> Result<u64> {
        growth(&self.db)
    }
    pub fn is_closed(&self, owner: &RuntimeOwner) -> Result<bool> {
        closed(&self.db, owner)
    }
    pub fn held_by_abort(&self, owner: &RuntimeOwner) -> Result<bool> {
        conflict(&self.db, owner)
    }
    pub fn claimed(&self, owner: &RuntimeOwner, runtime: &str) -> Result<bool> {
        Ok(self.db.query_row("SELECT EXISTS(SELECT 1 FROM native_transfer_claim WHERE (kind='owner' AND key=?1) OR (kind='runtime' AND key=?2))",params![owner_key(owner)?,runtime],|row|row.get(0))?)
    }
    pub fn claimed_audit(&self, audit: &str) -> Result<bool> {
        Ok(self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM native_transfer_claim WHERE kind='audit' AND key=?1)",
            [audit],
            |row| row.get(0),
        )?)
    }
    pub fn claimed_log(&self, log: &Path) -> Result<bool> {
        Ok(self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM native_transfer_claim WHERE kind='audit-file' AND key=?1)",
            [log.to_str().context("non-UTF8 native audit log")?],
            |row| row.get(0),
        )?)
    }
    pub fn claimed_anchor(&self, anchor: &Path) -> Result<bool> {
        Ok(self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM native_transfer_claim WHERE kind='anchor' AND key=?1)",
            [anchor.to_str().context("non-UTF8 native audit anchor")?],
            |row| row.get(0),
        )?)
    }
    pub fn prepare(
        &mut self,
        request: &TransferRequest,
        endpoints: &[Endpoint],
        max_state_bytes: u64,
    ) -> Result<State> {
        request.validate().context(R::InvalidRequest)?;
        let claims = claim_set(request, endpoints)?;
        let request_bytes = encoded(request)?;
        if let Some(prior) = load(&self.db, &request.transfer_id)? {
            ensure!(
                prior.request == *request && prior.claims == claims,
                R::AuthorityConflict
            );
            return Ok(prior.state);
        }
        for owner in request
            .source_owners
            .iter()
            .chain(std::iter::once(&request.destination))
        {
            ensure!(
                !closed(&self.db, &owner.owner)? && !conflict(&self.db, &owner.owner)?,
                R::AuthorityConflict
            );
        }
        for claim in &claims {
            ensure!(
                !self.db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM native_transfer_claim WHERE kind=?1 AND key=?2)",
                    params![claim.kind, claim.key],
                    |row| row.get::<_, bool>(0)
                )?,
                R::LockBusy
            );
        }
        let reserved = reservation(&request_bytes, &encoded(&claims)?)?;
        ensure!(reserved <= HISTORY_LIMIT, R::InvalidRequest);
        let outstanding = growth(&self.db)?;
        let mut paths: Vec<_> = request
            .source_owners
            .iter()
            .map(|source| source.audit_path.clone())
            .collect();
        paths.sort();
        paths.dedup();
        let preview = TransferReceipt::for_request(
            request,
            paths
                .into_iter()
                .map(|audit_path| SealReference {
                    audit_path,
                    seal_sha256: "0".repeat(64),
                })
                .collect(),
        )?;
        ensure!(
            encoded(&preview)?.len() <= RECEIPT_LIMIT
                && encoded(&preview.source_seals)?.len() <= SEALS_LIMIT,
            R::InvalidRequest
        );
        ensure!(
            history_bytes(self.root)?
                .checked_add(outstanding)
                .and_then(|bytes| bytes.checked_add(reserved))
                .is_some_and(|bytes| bytes <= HISTORY_LIMIT),
            R::HistoryCapacity
        );
        ensure!(
            super::tree_bytes(self.root)?
                .checked_add(outstanding)
                .and_then(|bytes| bytes.checked_add(reserved))
                .is_some_and(|bytes| bytes <= max_state_bytes)
                && super::available_bytes(self.root)?
                    >= super::SESSION_BYTES
                        .saturating_add(outstanding)
                        .saturating_add(reserved),
            R::StateCapacity
        );
        let transaction = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let claim_bytes = encoded(&claims)?;
        transaction.execute("INSERT INTO native_transfer(id,request_json,request_sha256,claims_json,claims_sha256,reserved_bytes,state) VALUES(?1,?2,?3,?4,?5,?6,'prepared')",params![request.transfer_id,request_bytes,request.request_digest()?,claim_bytes,sha(&claim_bytes),reserved])?;
        for source in &request.source_owners {
            transaction.execute(
                "INSERT INTO native_transfer_source VALUES(?1,?2,?3,?4)",
                params![
                    request.transfer_id,
                    source.owner.workflow_id,
                    source.owner.lease_generation,
                    source.audit_path
                ],
            )?;
        }
        for claim in claims {
            transaction.execute(
                "INSERT INTO native_transfer_claim VALUES(?1,?2,?3)",
                params![claim.kind, claim.key, request.transfer_id],
            )?;
        }
        transaction.commit()?;
        Ok(State::Prepared)
    }
    pub fn commit(
        &mut self,
        request: &TransferRequest,
        seals: Vec<SealReference>,
    ) -> Result<TransferReceipt> {
        let expected = TransferReceipt::for_request(request, seals)?;
        if let Some(record) = load(&self.db, &request.transfer_id)? {
            ensure!(record.request == *request, R::AuthorityConflict);
            if let State::Completed(receipt) = record.state {
                ensure!(receipt == expected, R::AuditCorrupt);
                return Ok(receipt);
            }
        }
        let transaction = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let record = load(&transaction, &request.transfer_id)?.context(R::PreparedCorrupt)?;
        ensure!(record.request == *request, R::AuthorityConflict);
        match record.state {
            State::Completed(receipt) => {
                ensure!(receipt == expected, R::AuditCorrupt);
                return Ok(receipt);
            }
            State::Aborted => anyhow::bail!(R::AuthorityConflict),
            State::Prepared => {}
        }
        for owner in request
            .source_owners
            .iter()
            .chain(std::iter::once(&request.destination))
        {
            ensure!(
                !closed(&transaction, &owner.owner)? && !conflict(&transaction, &owner.owner)?,
                R::AuthorityConflict
            );
        }
        for row in &request.inventory {
            let source = resolve(
                &transaction,
                &row.runtime_relative_path,
                &Authority {
                    owner: row.creation_owner.clone(),
                    audit_path: row.creation_audit_path.clone(),
                },
                &row.creation_receipt_sha256,
            )?;
            ensure!(
                source.owner == row.effective_owner
                    && source.audit_path == row.effective_audit_path,
                R::AuthorityConflict
            );
        }
        let bytes = encoded(&expected)?;
        let seals = encoded(&expected.source_seals)?;
        ensure!(
            bytes.len() <= RECEIPT_LIMIT && seals.len() <= SEALS_LIMIT,
            R::HistoryCapacity
        );
        for (index, row) in request.inventory.iter().enumerate() {
            transaction.execute(
                "INSERT INTO native_transfer_link(transfer_id,ordinal,runtime) VALUES(?1,?2,?3)",
                params![request.transfer_id, index, row.runtime_relative_path],
            )?;
        }
        for source in &request.source_owners {
            transaction.execute(
                "INSERT INTO native_transfer_close VALUES(?1,?2,?3,?4)",
                params![
                    source.owner.workflow_id,
                    source.owner.lease_generation,
                    source.audit_path,
                    request.transfer_id
                ],
            )?;
        }
        transaction.execute("UPDATE native_transfer SET state='completed',receipt_json=?2,seals_json=?3 WHERE id=?1",params![request.transfer_id,bytes,seals])?;
        transaction.execute(
            "DELETE FROM native_transfer_claim WHERE transfer_id=?1",
            [&request.transfer_id],
        )?;
        transaction.commit()?;
        Ok(expected)
    }
    pub fn resolve(
        &self,
        runtime: &str,
        creation: &Authority,
        creation_sha256: &str,
    ) -> Result<Authority> {
        resolve(&self.db, runtime, creation, creation_sha256)
    }
    /// Only the existing fenced controller retirement entry point may call this.
    pub fn abort_and_retire(&mut self, audit_path: &str, owner: &RuntimeOwner) -> Result<()> {
        ensure!(
            Path::new(audit_path).is_absolute()
                && !owner.workflow_id.is_empty()
                && owner.lease_generation >= 0,
            R::InvalidRequest
        );
        let transaction = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        growth(&transaction)?; // Corrupt pending state never grants retirement authority.
        let actual = match Path::new(audit_path).canonicalize() {
            Ok(path) => Some(
                path.to_str()
                    .context("non-UTF8 native audit log")?
                    .to_string(),
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let mut query=transaction.prepare("SELECT DISTINCT transfer_id FROM native_transfer_claim WHERE (kind='owner' AND key=?1) OR (kind='audit' AND key=?2) OR (kind='audit-file' AND key=?3)")?;
        let ids = query
            .query_map(params![owner_key(owner)?, audit_path, actual], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(query);
        for id in &ids {
            ensure!(
                matches!(
                    load(&transaction, id)?.context(R::PreparedCorrupt)?.state,
                    State::Prepared
                ),
                R::PreparedCorrupt
            );
        }
        for id in ids {
            transaction.execute(
                "UPDATE native_transfer SET state='aborted' WHERE id=?1",
                [&id],
            )?;
            transaction.execute(
                "DELETE FROM native_transfer_claim WHERE transfer_id=?1",
                [&id],
            )?;
        }
        transaction.execute(
            "INSERT OR IGNORE INTO retired_owner VALUES(?1,?2,?3)",
            params![audit_path, owner.workflow_id, owner.lease_generation],
        )?;
        transaction.commit()?;
        Ok(())
    }
}
fn resolve(
    db: &rusqlite::Connection,
    runtime: &str,
    creation: &Authority,
    creation_sha256: &str,
) -> Result<Authority> {
    let mut authority = creation.clone();
    let mut query = db.prepare(
        "SELECT transfer_id,ordinal FROM native_transfer_link WHERE runtime=?1 ORDER BY seq",
    )?;
    let links = query
        .query_map([runtime], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, usize>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (id, index) in links {
        let record = load(db, &id)?.context(R::PreparedCorrupt)?;
        let State::Completed(receipt) = record.state else {
            anyhow::bail!(R::PreparedCorrupt)
        };
        let row = record
            .request
            .inventory
            .get(index)
            .context(R::PreparedCorrupt)?;
        ensure!(
            row.runtime_relative_path == runtime
                && row.creation_owner == creation.owner
                && row.creation_audit_path == creation.audit_path
                && row.creation_receipt_sha256 == creation_sha256
                && row.effective_owner == authority.owner
                && row.effective_audit_path == authority.audit_path,
            R::MetadataInvalid
        );
        authority = receipt.destination;
    }
    ensure!(!transfer_closed(db, &authority.owner)?, R::PreparedCorrupt);
    Ok(authority)
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn native_store_identity_missing_ledger_never_recreates_prepared_authority() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        super::super::retirement_db(root.path()).unwrap();
        let lock = StateLock::acquire(root.path()).unwrap();
        let request = request("workflow");
        Ledger::open(root.path(), &lock)
            .unwrap()
            .prepare(&request, &endpoints(&request), u64::MAX)
            .unwrap();
        let db = root.path().join("runtime-retirement.sqlite");
        std::fs::remove_file(&db).unwrap();
        assert!(
            Ledger::open(root.path(), &lock).is_err(),
            "missing prepared authority was silently recreated"
        );
        assert!(
            !db.exists(),
            "refusal created a replacement authority database"
        );
    }
    #[test]
    fn native_store_identity_replacement_is_rejected_before_sqlite_mutates_it() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        super::super::retirement_db(root.path()).unwrap();
        let lock = StateLock::acquire(root.path()).unwrap();
        let request = request("workflow");
        Ledger::open(root.path(), &lock)
            .unwrap()
            .prepare(&request, &endpoints(&request), u64::MAX)
            .unwrap();
        let db = root.path().join("runtime-retirement.sqlite");
        let retained = tempfile::tempdir().unwrap();
        std::fs::rename(&db, retained.path().join("original.sqlite")).unwrap();
        super::super::write(&db, b"").unwrap();
        assert!(
            Ledger::open(root.path(), &lock).is_err(),
            "replacement ledger became fresh authority"
        );
        assert_eq!(
            std::fs::metadata(db).unwrap().len(),
            0,
            "refusal modified the replacement file"
        );
    }
    #[test]
    fn native_store_identity_bare_root_does_not_imply_fresh_authority() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let lock = StateLock::acquire(root.path()).unwrap();
        assert!(
            Ledger::open(root.path(), &lock).is_err(),
            "empty directory was treated as permission to bootstrap authority"
        );
        assert!(!root.path().join("runtime-retirement.sqlite").exists());
    }
    #[test]
    fn native_store_identity_verified_legacy_migration_keeps_closures_and_epoch() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        super::super::retirement_db(root.path())
            .unwrap()
            .execute(
                "INSERT INTO retired_owner VALUES('/audit/original','workflow',1)",
                [],
            )
            .unwrap();
        let owner = RuntimeOwner {
            workflow_id: "workflow".into(),
            lease_generation: 1,
        };
        let lock = StateLock::acquire(root.path()).unwrap();
        assert!(Ledger::open(root.path(), &lock)
            .unwrap()
            .is_closed(&owner)
            .unwrap());
        let epoch = std::fs::read(root.path().join("authority-epoch.json"))
            .expect("legacy migration published no durable epoch");
        assert!(Ledger::open(root.path(), &lock)
            .unwrap()
            .is_closed(&owner)
            .unwrap());
        assert_eq!(
            std::fs::read(root.path().join("authority-epoch.json")).unwrap(),
            epoch
        );
    }
    fn migration_refuses_terminal_corruption(completed: bool) {
        let root = fixture();
        let lock = StateLock::acquire(root.path()).unwrap();
        let request = request("workflow");
        let mut ledger = Ledger::open(root.path(), &lock).unwrap();
        ledger
            .prepare(&request, &endpoints(&request), u64::MAX)
            .unwrap();
        if completed {
            ledger.commit(&request, seals(&request)).unwrap();
            ledger
                .db
                .execute(
                    "UPDATE native_transfer SET receipt_json=?1 WHERE id=?2",
                    params![b"{}".to_vec(), request.transfer_id],
                )
                .unwrap();
        } else {
            ledger
                .abort_and_retire(
                    &request.source_owners[0].audit_path,
                    &request.source_owners[0].owner,
                )
                .unwrap();
            ledger
                .db
                .execute(
                    "UPDATE native_transfer SET request_sha256='changed' WHERE id=?1",
                    [&request.transfer_id],
                )
                .unwrap();
        }
        ledger
            .db
            .execute_batch("DROP TABLE native_authority_epoch")
            .unwrap();
        std::fs::remove_file(root.path().join("authority-epoch.json")).unwrap();
        drop(ledger);
        assert!(
            Ledger::open(root.path(), &lock).is_err(),
            "corrupt terminal history became active authority"
        );
        let db = rusqlite::Connection::open(root.path().join("runtime-retirement.sqlite")).unwrap();
        assert_eq!(
            db.query_row(
                "SELECT state FROM native_authority_epoch WHERE id=1",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
            "initializing"
        );
    }
    #[test]
    fn native_store_identity_migration_validates_completed_history_before_activation() {
        migration_refuses_terminal_corruption(true);
    }
    #[test]
    fn native_store_identity_migration_validates_aborted_history_before_activation() {
        migration_refuses_terminal_corruption(false);
    }
    fn epoch_schema_refused(extra: bool) {
        let root = fixture();
        let lock = StateLock::acquire(root.path()).unwrap();
        let ledger = Ledger::open(root.path(), &lock).unwrap();
        if extra {
            ledger
                .db
                .execute_batch("ALTER TABLE native_authority_epoch ADD COLUMN extra TEXT")
                .unwrap();
        } else {
            ledger.db.execute_batch("ALTER TABLE native_authority_epoch RENAME TO previous_epoch;
            CREATE TABLE native_authority_epoch(id INTEGER PRIMARY KEY,version INTEGER NOT NULL,epoch TEXT NOT NULL,root_identity_sha256 TEXT NOT NULL,ledger_identity_sha256 TEXT NOT NULL,state TEXT NOT NULL,origin TEXT NOT NULL);
            INSERT INTO native_authority_epoch SELECT * FROM previous_epoch; DROP TABLE previous_epoch;").unwrap();
        }
        drop(ledger);
        assert!(
            Ledger::open(root.path(), &lock).is_err(),
            "altered authority schema was accepted"
        );
    }
    #[test]
    fn native_store_identity_extra_epoch_columns_refuse() {
        epoch_schema_refused(true);
    }
    #[test]
    fn native_store_identity_missing_epoch_constraints_refuse() {
        epoch_schema_refused(false);
    }
    fn request(workflow: &str) -> TransferRequest {
        let source = RuntimeOwner {
            workflow_id: workflow.into(),
            lease_generation: 1,
        };
        serde_json::from_value(json!({"version":1,"transfer_id":uuid::Uuid::new_v4().to_string(),"workflow_id":workflow,
            "source_owners":[{"owner":source,"audit_path":format!("/audit/{workflow}/source")}],
            "destination":{"owner":{"workflow_id":workflow,"lease_generation":2},"audit_path":format!("/audit/{workflow}/destination")},
            "inventory":[{"provider":"codex","native_session_id":format!("native-{workflow}"),"runtime_relative_path":format!("codex/session-{workflow}"),
                "creation_owner":source,"creation_audit_path":format!("/audit/{workflow}/source"),
                "effective_owner":source,"effective_audit_path":format!("/audit/{workflow}/source"),
                "creation_receipt_sha256":"a".repeat(64),"completion_sha256":"b".repeat(64),"alias_sha256":"c".repeat(64),"export_manifest_sha256":"d".repeat(64)}]})).unwrap()
    }
    fn endpoints(request: &TransferRequest) -> Vec<Endpoint> {
        request
            .source_owners
            .iter()
            .chain(std::iter::once(&request.destination))
            .map(|owner| Endpoint {
                audit_path: owner.audit_path.clone(),
                actual_audit_path: owner.audit_path.clone(),
                anchor_path: format!("{}.anchor", owner.audit_path),
            })
            .collect()
    }
    fn seals(request: &TransferRequest) -> Vec<SealReference> {
        let mut paths: Vec<_> = request
            .source_owners
            .iter()
            .map(|owner| owner.audit_path.clone())
            .collect();
        paths.sort();
        paths.dedup();
        paths
            .into_iter()
            .map(|audit_path| SealReference {
                audit_path,
                seal_sha256: "e".repeat(64),
            })
            .collect()
    }
    #[test]
    fn native_ledger_preparation_survives_reopen_and_rejects_rebinding() {
        let root = fixture();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let lock = StateLock::acquire(root.path()).unwrap();
        let request = request("workflow");
        {
            let mut ledger = Ledger::open(root.path(), &lock).unwrap();
            assert_eq!(
                ledger
                    .prepare(&request, &endpoints(&request), u64::MAX)
                    .unwrap(),
                State::Prepared
            );
        }
        let mut ledger = Ledger::open(root.path(), &lock).unwrap();
        assert_eq!(
            ledger.state(&request.transfer_id).unwrap(),
            Some(State::Prepared)
        );
        assert_eq!(
            ledger
                .prepare(&request, &endpoints(&request), u64::MAX)
                .unwrap(),
            State::Prepared
        );
        let mut changed = request.clone();
        changed.destination.audit_path.push_str("-other");
        assert!(ledger
            .prepare(&changed, &endpoints(&changed), u64::MAX)
            .is_err());
    }
    #[test]
    fn native_ledger_completed_replay_never_downgrades_a_later_transfer() {
        let root = fixture();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let lock = StateLock::acquire(root.path()).unwrap();
        let first = request("workflow");
        let creation = first.source_owners[0].clone();
        let row = &first.inventory[0];
        let mut ledger = Ledger::open(root.path(), &lock).unwrap();
        ledger
            .prepare(&first, &endpoints(&first), u64::MAX)
            .unwrap();
        let receipt = ledger.commit(&first, seals(&first)).unwrap();
        receipt.verify(&first).unwrap();
        assert_eq!(
            ledger
                .resolve(
                    &row.runtime_relative_path,
                    &creation,
                    &row.creation_receipt_sha256
                )
                .unwrap(),
            first.destination
        );
        let mut next = first.clone();
        next.transfer_id = uuid::Uuid::new_v4().to_string();
        next.source_owners = vec![first.destination.clone()];
        next.destination.owner.lease_generation = 3;
        next.destination.audit_path.push_str("-newer");
        next.inventory[0].effective_owner = first.destination.owner.clone();
        next.inventory[0].effective_audit_path = first.destination.audit_path.clone();
        ledger.prepare(&next, &endpoints(&next), u64::MAX).unwrap();
        ledger.commit(&next, seals(&next)).unwrap();
        assert_eq!(ledger.commit(&first, seals(&first)).unwrap(), receipt);
        assert_eq!(
            ledger
                .resolve(
                    &row.runtime_relative_path,
                    &creation,
                    &row.creation_receipt_sha256
                )
                .unwrap(),
            next.destination
        );
    }
    fn fixture() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        super::super::initialize_native_test_store(root.path());
        root
    }
    #[test]
    fn native_ledger_claims_both_endpoints_and_allows_disjoint_workflows() {
        let root = fixture();
        let lock = StateLock::acquire(root.path()).unwrap();
        let first = request("first");
        let mut ledger = Ledger::open(root.path(), &lock).unwrap();
        ledger
            .prepare(&first, &endpoints(&first), u64::MAX)
            .unwrap();
        let reserved = ledger.reserved_bytes().unwrap();
        assert!(reserved > 0);
        assert!(ledger
            .claimed(
                &first.source_owners[0].owner,
                &first.inventory[0].runtime_relative_path
            )
            .unwrap());
        assert!(ledger.claimed(&first.destination.owner, "absent").unwrap());
        let mut overlap = first.clone();
        overlap.transfer_id = uuid::Uuid::new_v4().to_string();
        overlap.source_owners[0].audit_path.push_str("-other");
        overlap.inventory.clear();
        assert_eq!(
            ledger
                .prepare(&overlap, &endpoints(&overlap), u64::MAX)
                .unwrap_err()
                .to_string(),
            "native-transfer-lock-busy"
        );
        assert!(ledger.state(&overlap.transfer_id).unwrap().is_none());
        assert_eq!(ledger.reserved_bytes().unwrap(), reserved);
        let second = request("second");
        ledger
            .prepare(&second, &endpoints(&second), u64::MAX)
            .unwrap();
        assert!(ledger.reserved_bytes().unwrap() > reserved);
    }
    #[test]
    fn native_ledger_abort_releases_growth_and_closes_only_requested_owner() {
        let root = fixture();
        let lock = StateLock::acquire(root.path()).unwrap();
        let mut pending = request("workflow");
        pending.destination.owner.lease_generation = 3;
        let peer = Authority {
            owner: RuntimeOwner {
                workflow_id: "workflow".into(),
                lease_generation: 2,
            },
            audit_path: "/audit/peer".into(),
        };
        pending.source_owners.push(peer.clone());
        let source = pending.source_owners[0].clone();
        let mut ledger = Ledger::open(root.path(), &lock).unwrap();
        ledger
            .prepare(&pending, &endpoints(&pending), u64::MAX)
            .unwrap();
        assert!(ledger.reserved_bytes().unwrap() > 0);
        ledger
            .abort_and_retire(&peer.audit_path, &peer.owner)
            .unwrap();
        assert_eq!(
            ledger.state(&pending.transfer_id).unwrap(),
            Some(State::Aborted)
        );
        assert_eq!(ledger.reserved_bytes().unwrap(), 0);
        assert!(ledger.is_closed(&peer.owner).unwrap());
        assert!(!ledger.is_closed(&source.owner).unwrap());
        assert!(!ledger.is_closed(&pending.destination.owner).unwrap());
        assert!(ledger.held_by_abort(&source.owner).unwrap());
        assert!(!ledger
            .claimed(&source.owner, &pending.inventory[0].runtime_relative_path)
            .unwrap());
        assert_eq!(
            ledger.prepare(&pending, &endpoints(&pending), 0).unwrap(),
            State::Aborted
        );
        assert!(ledger.commit(&pending, seals(&pending)).is_err());
        let mut replacement = pending.clone();
        replacement.transfer_id = uuid::Uuid::new_v4().to_string();
        assert!(ledger
            .prepare(&replacement, &endpoints(&replacement), u64::MAX)
            .is_err());
        assert_eq!(
            ledger
                .resolve(
                    &pending.inventory[0].runtime_relative_path,
                    &source,
                    &pending.inventory[0].creation_receipt_sha256
                )
                .unwrap(),
            source
        );
    }
    #[test]
    fn native_ledger_last_write_failure_rolls_back_links_closures_and_claim_release() {
        let root = fixture();
        let lock = StateLock::acquire(root.path()).unwrap();
        let request = request("workflow");
        let source = request.source_owners[0].clone();
        let mut ledger = Ledger::open(root.path(), &lock).unwrap();
        ledger
            .prepare(&request, &endpoints(&request), u64::MAX)
            .unwrap();
        let reserved = ledger.reserved_bytes().unwrap();
        ledger.db.execute_batch("CREATE TRIGGER injected_commit_failure BEFORE INSERT ON native_transfer_close BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
        assert!(ledger.commit(&request, seals(&request)).is_err());
        assert_eq!(
            ledger.state(&request.transfer_id).unwrap(),
            Some(State::Prepared)
        );
        assert_eq!(ledger.reserved_bytes().unwrap(), reserved);
        assert!(!ledger.is_closed(&source.owner).unwrap());
        assert_eq!(
            ledger
                .resolve(
                    &request.inventory[0].runtime_relative_path,
                    &source,
                    &request.inventory[0].creation_receipt_sha256
                )
                .unwrap(),
            source
        );
        ledger
            .db
            .execute_batch("DROP TRIGGER injected_commit_failure")
            .unwrap();
        ledger.commit(&request, seals(&request)).unwrap();
        assert_eq!(ledger.reserved_bytes().unwrap(), 0);
    }
    #[test]
    fn native_ledger_corrupt_pending_claims_refuse_fenced_retirement_atomically() {
        let root = fixture();
        let lock = StateLock::acquire(root.path()).unwrap();
        let request = request("workflow");
        let source = &request.source_owners[0];
        let mut ledger = Ledger::open(root.path(), &lock).unwrap();
        ledger
            .prepare(&request, &endpoints(&request), u64::MAX)
            .unwrap();
        // Delete both matching claims: retirement must validate all prepared
        // records, rather than inspecting only whichever claims remain.
        ledger.db.execute("DELETE FROM native_transfer_claim WHERE (kind='owner' AND key=?1) OR (kind='audit' AND key=?2)",params![owner_key(&source.owner).unwrap(),source.audit_path]).unwrap();
        assert!(ledger
            .abort_and_retire(&source.audit_path, &source.owner)
            .is_err());
        assert_eq!(
            ledger
                .db
                .query_row("SELECT count(*) FROM retired_owner", [], |row| row
                    .get::<_, u64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            ledger
                .db
                .query_row(
                    "SELECT state FROM native_transfer WHERE id=?1",
                    [&request.transfer_id],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            "prepared"
        );
    }
    #[test]
    fn native_ledger_capacity_includes_crash_surviving_reservations_and_releases_on_abort() {
        let root = fixture();
        let lock = StateLock::acquire(root.path()).unwrap();
        let mut ledger = Ledger::open(root.path(), &lock).unwrap();
        let first = request("capacity-0");
        assert!(ledger.prepare(&first, &endpoints(&first), 0).is_err());
        assert!(ledger.state(&first.transfer_id).unwrap().is_none());
        let mut admitted = Vec::new();
        let mut refused = None;
        for index in 0..50 {
            let candidate = if index == 0 {
                first.clone()
            } else {
                request(&format!("capacity-{index}"))
            };
            match ledger.prepare(&candidate, &endpoints(&candidate), u64::MAX) {
                Ok(State::Prepared) => admitted.push(candidate),
                Err(error) => {
                    assert_eq!(error.to_string(), "native-transfer-history-capacity");
                    refused = Some(candidate);
                    break;
                }
                other => panic!("unexpected preparation: {other:?}"),
            }
        }
        let refused = refused.expect("unresolved reservations must exhaust bounded history");
        assert!(!admitted.is_empty());
        let before = ledger.reserved_bytes().unwrap();
        drop(ledger);
        let mut ledger = Ledger::open(root.path(), &lock).unwrap();
        assert_eq!(ledger.reserved_bytes().unwrap(), before);
        assert_eq!(
            ledger
                .prepare(&admitted[0], &endpoints(&admitted[0]), 0)
                .unwrap(),
            State::Prepared
        );
        assert_eq!(ledger.reserved_bytes().unwrap(), before);
        let source = &admitted[0].source_owners[0];
        ledger
            .abort_and_retire(&source.audit_path, &source.owner)
            .unwrap();
        assert!(ledger.reserved_bytes().unwrap() < before);
        assert_eq!(
            ledger
                .prepare(&refused, &endpoints(&refused), u64::MAX)
                .unwrap(),
            State::Prepared
        );
    }
    #[test]
    fn native_ledger_receipt_and_creation_corruption_never_grant_authority() {
        let root = fixture();
        let lock = StateLock::acquire(root.path()).unwrap();
        let request = request("workflow");
        let source = &request.source_owners[0];
        let mut ledger = Ledger::open(root.path(), &lock).unwrap();
        ledger
            .prepare(&request, &endpoints(&request), u64::MAX)
            .unwrap();
        let receipt = ledger.commit(&request, seals(&request)).unwrap();
        let mut changed = receipt.clone();
        changed.mappings[0].effective_owner.lease_generation = 1;
        assert!(changed.verify(&request).is_err());
        assert!(ledger
            .resolve(
                &request.inventory[0].runtime_relative_path,
                source,
                &"0".repeat(64)
            )
            .is_err());
        // Missing latest link must refuse rather than revive the closed source.
        ledger
            .db
            .execute(
                "DELETE FROM native_transfer_link WHERE transfer_id=?1",
                [&request.transfer_id],
            )
            .unwrap();
        assert!(ledger
            .resolve(
                &request.inventory[0].runtime_relative_path,
                source,
                &request.inventory[0].creation_receipt_sha256
            )
            .is_err());
        assert!(ledger.state(&request.transfer_id).is_err());
    }

    fn physical_runtime(root: &Path, request: &mut TransferRequest) -> std::path::PathBuf {
        let row = &mut request.inventory[0];
        let path = root.join(&row.runtime_relative_path);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let bytes = serde_json::to_vec(
            &json!({"version":1,"owner":row.creation_owner,"audit_path":row.creation_audit_path}),
        )
        .unwrap();
        super::super::write(&path.join("owned.json"), &bytes).unwrap();
        row.creation_receipt_sha256 = sha(&bytes);
        super::super::write(&path.join("complete.json"),br#"{"version":1,"exit_code":0,"generation":"generation-one","worker_quiescence":{"remaining_workers":0,"quiet_scans":2}}"#).unwrap();
        path
    }
    #[test]
    fn native_ledger_existing_fenced_retirement_aborts_prepared_handoff() {
        let root = fixture();
        let lock = StateLock::acquire(root.path()).unwrap();
        let request = request("workflow");
        let source = &request.source_owners[0];
        let mut ledger = Ledger::open(root.path(), &lock).unwrap();
        ledger
            .prepare(&request, &endpoints(&request), u64::MAX)
            .unwrap();
        super::super::retire_owned_at(
            root.path(),
            &lock,
            Path::new(&source.audit_path),
            &source.owner,
        )
        .unwrap();
        assert_eq!(
            ledger.state(&request.transfer_id).unwrap(),
            Some(State::Aborted)
        );
        assert_eq!(ledger.reserved_bytes().unwrap(), 0);
    }
    #[test]
    fn native_ledger_resume_and_terminal_retirement_use_effective_owner() {
        let root = fixture();
        let lock = StateLock::acquire(root.path()).unwrap();
        let mut request = request("workflow");
        let runtime = physical_runtime(root.path(), &mut request);
        let original = std::fs::read(runtime.join("owned.json")).unwrap();
        let source = &request.source_owners[0];
        let destination = &request.destination;
        let mut ledger = Ledger::open(root.path(), &lock).unwrap();
        ledger
            .prepare(&request, &endpoints(&request), u64::MAX)
            .unwrap();
        ledger.commit(&request, seals(&request)).unwrap();
        assert!(
            super::super::validate_resume_owner(
                root.path(),
                &lock,
                &runtime,
                Path::new(&destination.audit_path),
                &destination.owner
            )
            .is_ok(),
            "destination cannot resume its transferred conversation"
        );
        assert_eq!(
            super::super::retire_owned_at(
                root.path(),
                &lock,
                Path::new(&source.audit_path),
                &source.owner
            )
            .unwrap(),
            0
        );
        assert!(!runtime.join("retired-owner.json").exists());
        assert_eq!(
            super::super::retire_owned_at(
                root.path(),
                &lock,
                Path::new(&destination.audit_path),
                &destination.owner
            )
            .unwrap(),
            1
        );
        assert!(runtime.join("retired-owner.json").is_file());
        assert_eq!(std::fs::read(runtime.join("owned.json")).unwrap(), original);
    }

    #[test]
    fn native_ledger_delayed_scope_exit_preserves_pending_transferred_and_aborted_peers() {
        for phase in ["prepared", "completed", "aborted-peer"] {
            let root = fixture();
            let lock = StateLock::acquire(root.path()).unwrap();
            let mut request = request("workflow");
            request.destination.owner.lease_generation = 3;
            let peer = Authority {
                owner: RuntimeOwner {
                    workflow_id: "workflow".into(),
                    lease_generation: 2,
                },
                audit_path: "/audit/peer".into(),
            };
            request.source_owners.push(peer.clone());
            let runtime = physical_runtime(root.path(), &mut request);
            let mut ledger = Ledger::open(root.path(), &lock).unwrap();
            ledger
                .prepare(&request, &endpoints(&request), u64::MAX)
                .unwrap();
            if phase == "completed" {
                ledger.commit(&request, seals(&request)).unwrap();
            }
            if phase == "aborted-peer" {
                ledger
                    .abort_and_retire(&peer.audit_path, &peer.owner)
                    .unwrap();
            }
            drop(ledger);
            drop(lock);
            super::super::retire(&super::super::Retirement {
                session: runtime.clone(),
                expected: request.source_owners[0].clone(),
            });
            assert!(
                !runtime.join("retired.json").exists(),
                "late source scope retired a {phase} runtime"
            );
        }
    }

    #[test]
    fn native_ledger_process_crash_replays_claims_and_lost_commit_receipts() {
        const TEST:&str="native_vm::owner_ledger::tests::native_ledger_process_crash_replays_claims_and_lost_commit_receipts";
        if let Some(root) = std::env::var_os("AUTOSHIP_LEDGER_CRASH_ROOT") {
            let root = std::path::PathBuf::from(root);
            let request =
                TransferRequest::parse(&super::super::private(&root.join("request.json")).unwrap())
                    .unwrap();
            let lock = StateLock::acquire(&root).unwrap();
            let mut ledger = Ledger::open(&root, &lock).unwrap();
            ledger
                .prepare(&request, &endpoints(&request), u64::MAX)
                .unwrap();
            if std::env::var("AUTOSHIP_LEDGER_CRASH_PHASE").unwrap() == "committed" {
                ledger.commit(&request, seals(&request)).unwrap();
            }
            // Simulate loss before any Rust/SQLite destructor or caller response.
            unsafe { libc::_exit(0) }
        }
        for phase in ["prepared", "committed"] {
            let root = fixture();
            let first = request("workflow");
            super::super::write(&root.path().join("request.json"), &encoded(&first).unwrap())
                .unwrap();
            let child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture"])
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("AUTOSHIP_LEDGER_CRASH_ROOT", root.path())
                .env("AUTOSHIP_LEDGER_CRASH_PHASE", phase)
                .output()
                .unwrap();
            assert!(
                child.status.success(),
                "crash fixture failed: {}",
                String::from_utf8_lossy(&child.stderr)
            );
            let lock = StateLock::acquire(root.path()).unwrap();
            let mut ledger = Ledger::open(root.path(), &lock).unwrap();
            if phase == "prepared" {
                assert_eq!(
                    ledger.state(&first.transfer_id).unwrap(),
                    Some(State::Prepared)
                );
                assert!(ledger.reserved_bytes().unwrap() > 0);
                assert!(ledger
                    .claimed(
                        &first.source_owners[0].owner,
                        &first.inventory[0].runtime_relative_path
                    )
                    .unwrap());
            } else {
                assert!(matches!(
                    ledger.state(&first.transfer_id).unwrap(),
                    Some(State::Completed(_))
                ));
                assert_eq!(ledger.reserved_bytes().unwrap(), 0);
            }
            let receipt = ledger.commit(&first, seals(&first)).unwrap();
            let mut next = first.clone();
            next.transfer_id = uuid::Uuid::new_v4().to_string();
            next.source_owners = vec![first.destination.clone()];
            next.destination.owner.lease_generation = 3;
            next.destination.audit_path.push_str("-newer");
            next.inventory[0].effective_owner = first.destination.owner.clone();
            next.inventory[0].effective_audit_path = first.destination.audit_path.clone();
            ledger.prepare(&next, &endpoints(&next), u64::MAX).unwrap();
            ledger.commit(&next, seals(&next)).unwrap();
            assert_eq!(
                ledger.prepare(&first, &endpoints(&first), 0).unwrap(),
                State::Completed(receipt.clone())
            );
            assert_eq!(ledger.commit(&first, seals(&first)).unwrap(), receipt);
            assert_eq!(
                ledger
                    .resolve(
                        &first.inventory[0].runtime_relative_path,
                        &first.source_owners[0],
                        &first.inventory[0].creation_receipt_sha256
                    )
                    .unwrap(),
                next.destination
            );
        }
    }
    #[test]
    fn native_ledger_late_alias_writer_cannot_publish_into_pending_or_transferred_runtime() {
        let root = fixture();
        let mut request = request("workflow");
        let runtime = physical_runtime(root.path(), &mut request);
        let aliases = root.path().join("codex/aliases");
        std::fs::create_dir(&aliases).unwrap();
        std::fs::set_permissions(&aliases, std::fs::Permissions::from_mode(0o700)).unwrap();
        let launch = super::super::Launch {
            command: tokio::process::Command::new("unused-fixture"),
            resource_policy: json!({}),
            aliases: aliases.clone(),
            session: runtime.clone(),
            ephemeral: false,
            authority: request.source_owners[0].clone(),
            audit_log: request.source_owners[0].audit_path.clone().into(),
            _directory: tempfile::tempdir().unwrap(),
        };
        launch.observe_session("observed-before").unwrap();
        {
            let lock = StateLock::acquire(root.path()).unwrap();
            Ledger::open(root.path(), &lock)
                .unwrap()
                .prepare(&request, &endpoints(&request), u64::MAX)
                .unwrap();
        }
        assert!(launch.observe_session("late-pending").is_err());
        assert_eq!(std::fs::read_dir(&aliases).unwrap().count(), 1);
        {
            let lock = StateLock::acquire(root.path()).unwrap();
            Ledger::open(root.path(), &lock)
                .unwrap()
                .commit(&request, seals(&request))
                .unwrap();
        }
        assert!(launch.observe_session("late-completed").is_err());
        assert_eq!(std::fs::read_dir(&aliases).unwrap().count(), 1);
        let destination = super::super::Launch {
            command: tokio::process::Command::new("unused-fixture"),
            resource_policy: json!({}),
            aliases: aliases.clone(),
            session: runtime,
            ephemeral: false,
            authority: request.destination.clone(),
            audit_log: request.destination.audit_path.clone().into(),
            _directory: tempfile::tempdir().unwrap(),
        };
        destination
            .observe_session("destination-observation")
            .unwrap();
        assert_eq!(std::fs::read_dir(&aliases).unwrap().count(), 2);
    }
    #[test]
    fn native_ledger_late_alias_writer_obeys_captured_actual_log_claim_after_alias_rebind() {
        use std::os::unix::fs::symlink;
        let root = fixture();
        let audit = fixture();
        let actual = audit.path().join("actual.log");
        let rebound = audit.path().join("rebound.log");
        super::super::write(&actual, b"original").unwrap();
        super::super::write(&rebound, b"replacement").unwrap();
        let alias = audit.path().join("alias.log");
        symlink(&actual, &alias).unwrap();
        let mut other = request("other");
        other.source_owners[0].audit_path = alias.to_str().unwrap().into();
        other.inventory[0].creation_audit_path = alias.to_str().unwrap().into();
        other.inventory[0].effective_audit_path = alias.to_str().unwrap().into();
        let runtime = physical_runtime(root.path(), &mut other);
        let aliases = root.path().join("codex/aliases");
        std::fs::create_dir(&aliases).unwrap();
        std::fs::set_permissions(&aliases, std::fs::Permissions::from_mode(0o700)).unwrap();
        let launch = super::super::Launch {
            command: tokio::process::Command::new("unused-fixture"),
            resource_policy: json!({}),
            aliases: aliases.clone(),
            session: runtime,
            ephemeral: false,
            authority: other.source_owners[0].clone(),
            audit_log: actual.clone(),
            _directory: tempfile::tempdir().unwrap(),
        };
        launch.observe_session("before-pending").unwrap();
        let mut pending = request("pending");
        pending.inventory.clear();
        let mut bindings = endpoints(&pending);
        bindings[0].actual_audit_path = actual.to_str().unwrap().into();
        {
            let lock = StateLock::acquire(root.path()).unwrap();
            Ledger::open(root.path(), &lock)
                .unwrap()
                .prepare(&pending, &bindings, u64::MAX)
                .unwrap();
        }
        std::fs::remove_file(&alias).unwrap();
        symlink(&rebound, &alias).unwrap();
        assert!(
            launch.observe_session("after-pending").is_err(),
            "late observer bypassed its captured log namespace claim"
        );
        assert_eq!(std::fs::read_dir(&aliases).unwrap().count(), 1);
    }
    #[test]
    fn native_ledger_real_audit_file_claims_cover_distinct_aliases_and_anchor_names() {
        let root = fixture();
        let lock = StateLock::acquire(root.path()).unwrap();
        let first = request("first");
        let second = request("second");
        let mut first_endpoints = endpoints(&first);
        first_endpoints[0].actual_audit_path = "/actual/shared/audit".into();
        let mut second_endpoints = endpoints(&second);
        second_endpoints[0].actual_audit_path = "/actual/shared/audit".into();
        let mut ledger = Ledger::open(root.path(), &lock).unwrap();
        ledger.prepare(&first, &first_endpoints, u64::MAX).unwrap();
        assert_eq!(
            ledger
                .prepare(&second, &second_endpoints, u64::MAX)
                .unwrap_err()
                .to_string(),
            "native-transfer-lock-busy"
        );
        assert!(ledger.state(&second.transfer_id).unwrap().is_none());
        let mut changed = endpoints(&first);
        changed[0].actual_audit_path = "/actual/rebound/audit".into();
        assert!(
            ledger.prepare(&first, &changed, u64::MAX).is_err(),
            "same request accepted rebound audit namespace"
        );
    }
    #[test]
    fn native_ledger_oversized_immutable_receipt_is_invalid_before_preparation() {
        let root = fixture();
        let lock = StateLock::acquire(root.path()).unwrap();
        let mut request = request("workflow");
        let source_path = format!("/audit/{}source", "segment/".repeat(250));
        let destination_path = format!("/audit/{}destination", "another/".repeat(250));
        request.source_owners = (0..64)
            .map(|lease_generation| Authority {
                owner: RuntimeOwner {
                    workflow_id: "workflow".into(),
                    lease_generation,
                },
                audit_path: source_path.clone(),
            })
            .collect();
        request.destination.owner.lease_generation = 64;
        request.destination.audit_path = destination_path;
        let row = request.inventory[0].clone();
        request.inventory = (0..64)
            .map(|index| {
                let mut row = row.clone();
                row.native_session_id = format!("native-{index}");
                row.runtime_relative_path = format!("codex/session-{index:03}");
                row.creation_audit_path = source_path.clone();
                row.effective_audit_path = source_path.clone();
                row
            })
            .collect();
        request.validate().unwrap();
        let mut ledger = Ledger::open(root.path(), &lock).unwrap();
        assert_eq!(
            ledger
                .prepare(&request, &endpoints(&request), u64::MAX)
                .unwrap_err()
                .to_string(),
            "native-transfer-invalid-request"
        );
        assert!(ledger.state(&request.transfer_id).unwrap().is_none());
        assert_eq!(ledger.reserved_bytes().unwrap(), 0);
        assert_eq!(
            ledger
                .db
                .query_row("SELECT count(*) FROM native_transfer_claim", [], |row| row
                    .get::<_, u64>(
                    0
                ))
                .unwrap(),
            0
        );
    }
    #[test]
    fn native_ledger_native_preparation_cannot_bypass_actual_log_claim_with_alias_and_alternate_anchor(
    ) {
        use std::os::unix::fs::symlink;
        let root = fixture();
        let audit = fixture();
        let logs = audit.path().join("logs");
        let anchors = audit.path().join("anchors");
        std::fs::create_dir(&logs).unwrap();
        std::fs::create_dir(&anchors).unwrap();
        let actual = logs.join("shared.jsonl");
        let source = crate::tool_audit::AuditLogger::protected(
            actual.clone(),
            anchors.join("source.json"),
            &[],
        )
        .unwrap();
        source
            .record_controller(json!({"actor":"controller","kind":"source"}))
            .unwrap();
        let alias = audit.path().join("alias");
        symlink(&logs, &alias).unwrap();
        let other = crate::tool_audit::AuditLogger::protected(
            alias.join("shared.jsonl"),
            anchors.join("other.json"),
            &[],
        )
        .unwrap();
        let owner = RuntimeOwner {
            workflow_id: "other".into(),
            lease_generation: 9,
        };
        other.set_native_runtime_owner(owner.clone()).unwrap();
        let mut request = request("first");
        request.source_owners[0].audit_path = actual.to_str().unwrap().into();
        request.inventory.clear();
        let mut endpoints = endpoints(&request);
        endpoints[0].anchor_path = source
            .protected_anchor_path()
            .unwrap()
            .to_str()
            .unwrap()
            .into();
        let lock = StateLock::acquire(root.path()).unwrap();
        let mut ledger = Ledger::open(root.path(), &lock).unwrap();
        ledger.prepare(&request, &endpoints, u64::MAX).unwrap();
        assert_eq!(
            super::super::ensure_native_admission(&ledger, &owner, &other)
                .unwrap_err()
                .to_string(),
            "native-transfer-lock-busy"
        );
    }
    #[test]
    fn native_ledger_fenced_retirement_intersects_canonical_actual_audit_alias() {
        use std::os::unix::fs::symlink;
        let root = fixture();
        let audit = fixture();
        let actual = audit.path().join("source.log");
        super::super::write(&actual, b"private fixture").unwrap();
        let alias = audit.path().join("alias.log");
        symlink(&actual, &alias).unwrap();
        let mut request = request("first");
        request.source_owners[0].audit_path = actual.to_str().unwrap().into();
        request.inventory.clear();
        let lock = StateLock::acquire(root.path()).unwrap();
        let mut ledger = Ledger::open(root.path(), &lock).unwrap();
        ledger
            .prepare(&request, &endpoints(&request), u64::MAX)
            .unwrap();
        let other = RuntimeOwner {
            workflow_id: "other".into(),
            lease_generation: 9,
        };
        ledger
            .abort_and_retire(alias.to_str().unwrap(), &other)
            .unwrap();
        assert_eq!(
            ledger.state(&request.transfer_id).unwrap(),
            Some(State::Aborted)
        );
        assert_eq!(ledger.reserved_bytes().unwrap(), 0);
        assert!(ledger.is_closed(&other).unwrap());
        assert!(!ledger.is_closed(&request.source_owners[0].owner).unwrap());
    }
    #[test]
    fn native_ledger_large_claim_bindings_use_exact_reserved_growth_instead_of_reference_limit() {
        let root = fixture();
        let lock = StateLock::acquire(root.path()).unwrap();
        let mut request = request("workflow");
        request.inventory.clear();
        request.source_owners = (0..64)
            .map(|lease_generation| Authority {
                owner: RuntimeOwner {
                    workflow_id: "workflow".into(),
                    lease_generation,
                },
                audit_path: format!("/declared/{lease_generation}"),
            })
            .collect();
        request.destination.owner.lease_generation = 64;
        let mut endpoints = endpoints(&request);
        for (index, endpoint) in endpoints.iter_mut().enumerate() {
            endpoint.actual_audit_path = format!("/actual/{index}/{}log", "segment/".repeat(500));
            endpoint.anchor_path = format!("/anchor/{index}/{}anchor", "segment/".repeat(500));
        }
        let claims = encoded(&claim_set(&request, &endpoints).unwrap()).unwrap();
        assert!(claims.len() > SEALS_LIMIT);
        {
            let mut ledger = Ledger::open(root.path(), &lock).unwrap();
            assert_eq!(
                ledger.prepare(&request, &endpoints, u64::MAX).unwrap(),
                State::Prepared
            );
            assert!(ledger.reserved_bytes().unwrap() > 2 * claims.len() as u64);
        }
        assert_eq!(
            Ledger::open(root.path(), &lock)
                .unwrap()
                .state(&request.transfer_id)
                .unwrap(),
            Some(State::Prepared)
        );
    }
}
