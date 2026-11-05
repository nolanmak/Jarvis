//! Shared wire bindings for healthy native-runtime ownership transfer.
//! These checks do not establish filesystem provenance, kernel quiescence, or
//! current ledger authority. Those remain mandatory in the transfer controller.
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

use crate::native_vm::RuntimeOwner;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Claude,
    Codex,
}
impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

fn strict_owner<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<RuntimeOwner, D::Error> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Owner {
        workflow_id: String,
        lease_generation: i64,
    }
    let owner = Owner::deserialize(deserializer)?;
    Ok(RuntimeOwner {
        workflow_id: owner.workflow_id,
        lease_generation: owner.lease_generation,
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OwnerAudit {
    #[serde(deserialize_with = "strict_owner")]
    pub owner: RuntimeOwner,
    pub audit_path: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryRow {
    pub provider: Provider,
    pub native_session_id: String,
    pub runtime_relative_path: String,
    #[serde(deserialize_with = "strict_owner")]
    pub creation_owner: RuntimeOwner,
    pub creation_audit_path: String,
    #[serde(deserialize_with = "strict_owner")]
    pub effective_owner: RuntimeOwner,
    pub effective_audit_path: String,
    pub creation_receipt_sha256: String,
    pub completion_sha256: String,
    pub alias_sha256: String,
    pub export_manifest_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferRequest {
    pub version: u32,
    pub transfer_id: String,
    pub workflow_id: String,
    pub source_owners: Vec<OwnerAudit>,
    pub destination: OwnerAudit,
    pub inventory: Vec<InventoryRow>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceClosure {
    #[serde(deserialize_with = "strict_owner")]
    pub owner: RuntimeOwner,
    pub audit_path: String,
    pub closed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceSeal {
    pub audit_path: String,
    pub seal_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectiveMapping {
    pub provider: Provider,
    pub native_session_sha256: String,
    pub runtime_relative_path: String,
    pub creation_receipt_sha256: String,
    pub completion_sha256: String,
    pub export_manifest_sha256: String,
    #[serde(deserialize_with = "strict_owner")]
    pub effective_owner: RuntimeOwner,
    pub effective_audit_path: String,
    pub transfer_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferReceipt {
    pub version: u32,
    pub transfer_id: String,
    pub request_sha256: String,
    pub source_inventory_sha256: String,
    pub source_closures: Vec<SourceClosure>,
    pub source_seals: Vec<SourceSeal>,
    pub destination: OwnerAudit,
    pub mappings: Vec<EffectiveMapping>,
    pub receipt_sha256: String,
}

pub fn canonical_bytes(value: &Value) -> Result<Vec<u8>> {
    fn append(value: &Value, out: &mut Vec<u8>, depth: usize) -> Result<()> {
        ensure!(
            depth <= 128,
            "native transfer JSON nesting exceeds parser limit"
        );
        match value {
            Value::Null => out.extend_from_slice(b"null"),
            Value::Bool(true) => out.extend_from_slice(b"true"),
            Value::Bool(false) => out.extend_from_slice(b"false"),
            Value::Number(number) => {
                ensure!(
                    number.is_i64() || number.is_u64(),
                    "native transfer canonical JSON forbids floats"
                );
                out.extend_from_slice(number.to_string().as_bytes());
            }
            Value::String(text) => serde_json::to_writer(&mut *out, text)?,
            Value::Array(values) => {
                out.push(b'[');
                for (index, value) in values.iter().enumerate() {
                    if index != 0 {
                        out.push(b',');
                    }
                    append(value, out, depth + 1)?;
                }
                out.push(b']');
            }
            Value::Object(values) => {
                let mut keys: Vec<_> = values.keys().collect();
                keys.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
                out.push(b'{');
                for (index, key) in keys.into_iter().enumerate() {
                    if index != 0 {
                        out.push(b',');
                    }
                    serde_json::to_writer(&mut *out, key)?;
                    out.push(b':');
                    append(&values[key], out, depth + 1)?;
                }
                out.push(b'}');
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    append(value, &mut out, 0)?;
    Ok(out)
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
fn absolute_audit(path: &str) -> bool {
    path.starts_with('/')
        && !path.contains('\0')
        && path
            .split('/')
            .skip(1)
            .all(|part| !part.is_empty() && part != "." && part != "..")
}
fn valid_owner(owner: &RuntimeOwner, workflow: &str) -> bool {
    owner.workflow_id == workflow && owner.lease_generation > 0
}
fn valid_runtime(path: &str, provider: Provider) -> bool {
    let parts: Vec<_> = path.split('/').collect();
    parts.len() == 2
        && parts[0] == provider.as_str()
        && parts[1].starts_with("session-")
        && parts[1].len() > "session-".len()
        && !parts[1].contains('\0')
}

impl TransferRequest {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let request: Self = serde_json::from_slice(bytes)?;
        request.validate()?;
        Ok(request)
    }
    pub fn request_digest(&self) -> Result<String> {
        request_digest(self)
    }
    pub fn inventory_digest(&self) -> Result<String> {
        inventory_digest(self)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1,
            "unsupported native transfer request version"
        );
        ensure!(
            !self.workflow_id.trim().is_empty(),
            "native transfer workflow missing"
        );
        ensure!(
            uuid::Uuid::parse_str(&self.transfer_id)?.to_string() == self.transfer_id,
            "native transfer ID must be a canonical UUID"
        );
        ensure!(
            self.source_owners.len() <= 64 && self.inventory.len() <= 64,
            "native-transfer-capacity: at most 64 source owners and sessions"
        );
        ensure!(
            !self.source_owners.is_empty(),
            "native transfer source owners missing"
        );
        ensure!(
            valid_owner(&self.destination.owner, &self.workflow_id)
                && absolute_audit(&self.destination.audit_path),
            "invalid native transfer destination"
        );
        for source in &self.source_owners {
            ensure!(
                valid_owner(&source.owner, &self.workflow_id) && absolute_audit(&source.audit_path),
                "invalid native transfer source owner"
            );
            ensure!(
                source.owner.lease_generation < self.destination.owner.lease_generation,
                "native transfer destination must be newer than every source"
            );
            ensure!(
                source.audit_path != self.destination.audit_path,
                "native transfer destination must use a distinct audit namespace"
            );
        }
        ensure!(
            self.source_owners.windows(2).all(|pair| {
                (
                    pair[0].owner.lease_generation,
                    pair[0].audit_path.as_bytes(),
                ) < (
                    pair[1].owner.lease_generation,
                    pair[1].audit_path.as_bytes(),
                )
            }),
            "native transfer sources must be sorted and unique"
        );
        ensure!(
            self.inventory.windows(2).all(|pair| {
                (pair[0].provider, pair[0].runtime_relative_path.as_bytes())
                    < (pair[1].provider, pair[1].runtime_relative_path.as_bytes())
            }),
            "native transfer inventory must be sorted and unique"
        );
        let mut paths = BTreeSet::new();
        let mut sessions = BTreeSet::new();
        for row in &self.inventory {
            ensure!(
                valid_runtime(&row.runtime_relative_path, row.provider),
                "invalid native runtime relative path"
            );
            ensure!(
                paths.insert(&row.runtime_relative_path)
                    && sessions.insert((row.provider, &row.native_session_id)),
                "duplicate native transfer runtime or session"
            );
            ensure!(
                !row.native_session_id.is_empty()
                    && row.native_session_id.len() <= 256
                    && row
                        .native_session_id
                        .bytes()
                        .all(|byte| byte.is_ascii_graphic()),
                "invalid observed native session ID"
            );
            ensure!(
                valid_owner(&row.creation_owner, &self.workflow_id)
                    && valid_owner(&row.effective_owner, &self.workflow_id)
                    && row.creation_owner.lease_generation <= row.effective_owner.lease_generation,
                "invalid native transfer creation/effective owner"
            );
            ensure!(
                absolute_audit(&row.creation_audit_path)
                    && absolute_audit(&row.effective_audit_path),
                "invalid native transfer inventory audit path"
            );
            ensure!(
                row.creation_owner != row.effective_owner
                    || row.creation_audit_path == row.effective_audit_path,
                "same native owner cannot change its audit namespace"
            );
            ensure!(
                self.source_owners
                    .iter()
                    .any(|source| source.owner == row.effective_owner
                        && source.audit_path == row.effective_audit_path),
                "native runtime effective owner omitted from source owners"
            );
            ensure!(
                [
                    &row.creation_receipt_sha256,
                    &row.completion_sha256,
                    &row.alias_sha256,
                    &row.export_manifest_sha256
                ]
                .into_iter()
                .all(|value| valid_hash(value)),
                "invalid native transfer inventory digest"
            );
        }
        Ok(())
    }
}
pub fn request_digest(request: &TransferRequest) -> Result<String> {
    request.validate()?;
    Ok(hash(&canonical_bytes(&serde_json::to_value(request)?)?))
}
pub fn inventory_digest(request: &TransferRequest) -> Result<String> {
    request.validate()?;
    Ok(hash(&canonical_bytes(
        &serde_json::json!({"version":1,"inventory":request.inventory}),
    )?))
}
pub fn receipt_digest(receipt: &TransferReceipt) -> Result<String> {
    let mut value = serde_json::to_value(receipt)?;
    value.as_object_mut().unwrap().remove("receipt_sha256");
    Ok(hash(&canonical_bytes(&value)?))
}
impl TransferReceipt {
    /// Historical binding only; dispatch additionally needs current ledger authority.
    pub fn verify(&self, request: &TransferRequest) -> Result<()> {
        verify_receipt(request, self)
    }
}
/// Verify the immutable wire binding, separately from current ledger authority.
pub fn verify_receipt(request: &TransferRequest, receipt: &TransferReceipt) -> Result<()> {
    request.validate()?;
    ensure!(
        canonical_bytes(&serde_json::to_value(receipt)?)?.len() <= 256 * 1024,
        "native-transfer-capacity: completed receipt exceeds 256 KiB"
    );
    ensure!(
        receipt.version == 1 && receipt.transfer_id == request.transfer_id,
        "native transfer receipt version/identity mismatch"
    );
    ensure!(
        receipt.request_sha256 == request_digest(request)?,
        "transfer request hash mismatch"
    );
    ensure!(
        receipt.source_inventory_sha256 == inventory_digest(request)?,
        "transfer inventory hash mismatch"
    );
    ensure!(
        receipt.destination == request.destination,
        "native transfer receipt destination mismatch"
    );
    ensure!(
        receipt.source_closures.len() == request.source_owners.len()
            && receipt
                .source_closures
                .iter()
                .zip(&request.source_owners)
                .all(|(closure, source)| {
                    closure.closed
                        && closure.owner == source.owner
                        && closure.audit_path == source.audit_path
                }),
        "native transfer receipt source closure mismatch"
    );
    let audits: BTreeSet<_> = request
        .source_owners
        .iter()
        .map(|source| &source.audit_path)
        .collect();
    ensure!(
        receipt.source_seals.len() == audits.len()
            && receipt
                .source_seals
                .windows(2)
                .all(|pair| pair[0].audit_path.as_bytes() < pair[1].audit_path.as_bytes())
            && receipt
                .source_seals
                .iter()
                .all(|seal| audits.contains(&seal.audit_path) && valid_hash(&seal.seal_sha256)),
        "native transfer receipt source seal coverage mismatch"
    );
    ensure!(
        receipt.mappings.len() == request.inventory.len(),
        "native transfer receipt incomplete runtime coverage"
    );
    for (mapping, row) in receipt.mappings.iter().zip(&request.inventory) {
        ensure!(
            mapping.provider == row.provider
                && mapping.native_session_sha256 == hash(row.native_session_id.as_bytes())
                && mapping.runtime_relative_path == row.runtime_relative_path
                && mapping.creation_receipt_sha256 == row.creation_receipt_sha256
                && mapping.completion_sha256 == row.completion_sha256
                && mapping.export_manifest_sha256 == row.export_manifest_sha256
                && mapping.effective_owner == request.destination.owner
                && mapping.effective_audit_path == request.destination.audit_path
                && mapping.transfer_id == request.transfer_id,
            "native transfer receipt runtime mapping mismatch"
        );
    }
    ensure!(
        receipt.receipt_sha256 == receipt_digest(receipt)?,
        "transfer receipt hash mismatch"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> TransferRequest {
        let source = OwnerAudit {
            owner: RuntimeOwner {
                workflow_id: "fixture/workflow".into(),
                lease_generation: 1,
            },
            audit_path: "/audit/source.jsonl".into(),
        };
        TransferRequest {
            version: 1,
            transfer_id: "128bc994-30c4-4ea1-aa2a-bad4308f7e9d".into(),
            workflow_id: source.owner.workflow_id.clone(),
            source_owners: vec![source.clone()],
            destination: OwnerAudit {
                owner: RuntimeOwner {
                    lease_generation: 2,
                    ..source.owner.clone()
                },
                audit_path: "/audit/destination.jsonl".into(),
            },
            inventory: [Provider::Claude, Provider::Codex]
                .into_iter()
                .map(|provider| InventoryRow {
                    provider,
                    native_session_id: format!("{}/native-1", provider.as_str()),
                    runtime_relative_path: format!("{}/session-runtime-1", provider.as_str()),
                    creation_owner: source.owner.clone(),
                    creation_audit_path: source.audit_path.clone(),
                    effective_owner: source.owner.clone(),
                    effective_audit_path: source.audit_path.clone(),
                    creation_receipt_sha256: "1".repeat(64),
                    completion_sha256: "2".repeat(64),
                    alias_sha256: "3".repeat(64),
                    export_manifest_sha256: "4".repeat(64),
                })
                .collect(),
        }
    }
    fn receipt(request: &TransferRequest) -> TransferReceipt {
        let mut receipt = TransferReceipt {
            version: 1,
            transfer_id: request.transfer_id.clone(),
            request_sha256: request_digest(request).unwrap(),
            source_inventory_sha256: inventory_digest(request).unwrap(),
            source_closures: request
                .source_owners
                .iter()
                .map(|source| SourceClosure {
                    owner: source.owner.clone(),
                    audit_path: source.audit_path.clone(),
                    closed: true,
                })
                .collect(),
            source_seals: vec![SourceSeal {
                audit_path: request.source_owners[0].audit_path.clone(),
                seal_sha256: "e".repeat(64),
            }],
            destination: request.destination.clone(),
            mappings: request
                .inventory
                .iter()
                .map(|row| EffectiveMapping {
                    provider: row.provider,
                    native_session_sha256: hash(row.native_session_id.as_bytes()),
                    runtime_relative_path: row.runtime_relative_path.clone(),
                    creation_receipt_sha256: row.creation_receipt_sha256.clone(),
                    completion_sha256: row.completion_sha256.clone(),
                    export_manifest_sha256: row.export_manifest_sha256.clone(),
                    effective_owner: request.destination.owner.clone(),
                    effective_audit_path: request.destination.audit_path.clone(),
                    transfer_id: request.transfer_id.clone(),
                })
                .collect(),
            receipt_sha256: String::new(),
        };
        receipt.receipt_sha256 = receipt_digest(&receipt).unwrap();
        receipt
    }

    #[test]
    fn canonical_bytes_preserve_literal_unicode_and_controls_and_reject_floats() {
        let value = serde_json::json!({"中":"😀","é":"e\u{301}","a/b":"\"\\\0\u{1}\u{8}\t\n\u{c}\r","a":[1,true,null]});
        let expected =
            r#"{"a":[1,true,null],"a/b":"\"\\\u0000\u0001\b\t\n\f\r","é":"é","中":"😀"}"#;
        assert_eq!(canonical_bytes(&value).unwrap(), expected.as_bytes());
        for value in [serde_json::json!(1.0), serde_json::json!({"nested":[1.5]})] {
            assert!(
                canonical_bytes(&value).is_err(),
                "floating point is not a canonical transfer value"
            );
        }
    }

    #[test]
    fn request_binds_complete_inventory_and_rejects_ambiguous_or_foreign_authority() {
        let request = fixture();
        request.validate().unwrap();
        let mut cases = Vec::new();
        let mut bad = request.clone();
        bad.inventory.reverse();
        cases.push(bad);
        let mut bad = request.clone();
        bad.inventory.push(bad.inventory[0].clone());
        cases.push(bad);
        let mut bad = request.clone();
        bad.source_owners.push(bad.source_owners[0].clone());
        cases.push(bad);
        let mut bad = request.clone();
        bad.source_owners.clear();
        cases.push(bad);
        let mut bad = request.clone();
        bad.destination.owner.lease_generation = 1;
        cases.push(bad);
        let mut bad = request.clone();
        bad.destination.owner.workflow_id = "other".into();
        cases.push(bad);
        let mut bad = request.clone();
        bad.destination.audit_path = request.source_owners[0].audit_path.clone();
        cases.push(bad);
        let mut bad = request.clone();
        bad.inventory[0].effective_audit_path = "/audit/unknown".into();
        cases.push(bad);
        let mut bad = request.clone();
        bad.inventory[0].creation_owner.lease_generation = 3;
        cases.push(bad);
        let mut bad = request.clone();
        bad.inventory[0].runtime_relative_path = "claude/session-x/../escape".into();
        cases.push(bad);
        let mut bad = request.clone();
        bad.inventory[0].native_session_id = "x".repeat(257);
        cases.push(bad);
        let mut bad = request.clone();
        bad.inventory[0].completion_sha256 = "A".repeat(64);
        cases.push(bad);
        let mut bad = request.clone();
        bad.transfer_id = request.transfer_id.to_uppercase();
        cases.push(bad);
        let mut bad = request.clone();
        bad.inventory = vec![bad.inventory[0].clone(); 65];
        cases.push(bad);
        for (index, bad) in cases.iter().enumerate() {
            assert!(
                bad.validate().is_err(),
                "invalid inventory case {index} accepted"
            );
        }
        let mut changed = request.clone();
        changed.inventory[0].alias_sha256 = "5".repeat(64);
        assert_ne!(
            request_digest(&request).unwrap(),
            request_digest(&changed).unwrap()
        );
        assert_ne!(
            inventory_digest(&request).unwrap(),
            inventory_digest(&changed).unwrap()
        );
    }

    #[test]
    fn a_self_consistent_receipt_cannot_omit_reorder_or_replace_request_authority() {
        let request = fixture();
        let good = receipt(&request);
        verify_receipt(&request, &good).unwrap();
        let mut cases = Vec::new();
        let mut bad = good.clone();
        bad.mappings.pop();
        cases.push(bad);
        let mut bad = good.clone();
        bad.mappings.reverse();
        cases.push(bad);
        let mut bad = good.clone();
        bad.mappings[0].native_session_sha256 = "a".repeat(64);
        cases.push(bad);
        let mut bad = good.clone();
        bad.mappings[0].completion_sha256 = "a".repeat(64);
        cases.push(bad);
        let mut bad = good.clone();
        bad.mappings[0].effective_owner.lease_generation = 1;
        cases.push(bad);
        let mut bad = good.clone();
        bad.destination.audit_path = "/audit/other".into();
        cases.push(bad);
        let mut bad = good.clone();
        bad.source_closures[0].closed = false;
        cases.push(bad);
        let mut bad = good.clone();
        bad.source_closures.clear();
        cases.push(bad);
        let mut bad = good.clone();
        bad.source_seals.clear();
        cases.push(bad);
        let mut bad = good.clone();
        bad.source_seals.push(bad.source_seals[0].clone());
        cases.push(bad);
        let mut bad = good.clone();
        bad.source_inventory_sha256 = "f".repeat(64);
        cases.push(bad);
        let mut bad = good.clone();
        bad.version = 2;
        cases.push(bad);
        for (index, mut bad) in cases.into_iter().enumerate() {
            bad.receipt_sha256 = receipt_digest(&bad).unwrap();
            assert!(
                verify_receipt(&request, &bad).is_err(),
                "self-consistent forgery case {index} accepted"
            );
        }
    }

    #[test]
    fn hashes_match_independent_python_compact_utf8_json_vectors() {
        // Generated independently with Python hashlib + json.dumps using
        // ensure_ascii=False, sort_keys=True, separators=(',', ':').
        let request = fixture();
        assert_eq!(
            request_digest(&request).unwrap(),
            "e69c988dea51b2cc0f78ccf9da333827b3cf7dcb20696de799b4fa024a5b71fb"
        );
        assert_eq!(
            inventory_digest(&request).unwrap(),
            "6502225707e60dce770a1c8df2e94f0ab2317879d29864bc5b05bf9437931a53"
        );
        let mut receipt = receipt(&request);
        assert_eq!(
            receipt_digest(&receipt).unwrap(),
            "bfa9ad12e84ff35c88d0433c7b2019b35f69db13f5717cfc78f71fec22d2b88c"
        );
        let hash = receipt_digest(&receipt).unwrap();
        receipt.receipt_sha256 = "not part of its own digest".into();
        assert_eq!(receipt_digest(&receipt).unwrap(), hash);
        assert!(verify_receipt(&request, &receipt).is_err());
    }

    #[test]
    fn wire_limits_are_inclusive_without_claiming_empty_filesystem_inventory() {
        let mut request = fixture();
        let row = request.inventory[0].clone();
        request.inventory = (0..64)
            .map(|index| InventoryRow {
                native_session_id: format!("opaque-{index}"),
                runtime_relative_path: format!("claude/session-{index:02}"),
                ..row.clone()
            })
            .collect();
        request.validate().unwrap();
        request.inventory.push(InventoryRow {
            native_session_id: "extra".into(),
            runtime_relative_path: "claude/session-64".into(),
            ..row
        });
        assert!(request
            .validate()
            .unwrap_err()
            .to_string()
            .contains("native-transfer-capacity"));
        // A transfer with no sessions still closes its explicit source owners.
        // The core scanner must separately prove there are no omitted runtimes.
        request.inventory.clear();
        request.validate().unwrap();
        verify_receipt(&request, &receipt(&request)).unwrap();
        let source = request.source_owners[0].clone();
        request.source_owners = (0..64)
            .map(|index| OwnerAudit {
                audit_path: format!("/audit/source-{index:02}.jsonl"),
                ..source.clone()
            })
            .collect();
        request.validate().unwrap();
        request.source_owners.push(OwnerAudit {
            audit_path: "/audit/extra.jsonl".into(),
            ..source
        });
        assert!(request
            .validate()
            .unwrap_err()
            .to_string()
            .contains("native-transfer-capacity"));
    }

    #[test]
    fn opaque_session_ids_are_not_paths_and_duplicate_aliases_are_refused() {
        let mut request = fixture();
        request.inventory[0].native_session_id = "x".repeat(256);
        request.validate().unwrap();
        for id in ["", "has space", "has\nnewline", "non-ascii-é", "nul\0byte"] {
            request.inventory[0].native_session_id = id.into();
            assert!(request.validate().is_err(), "invalid opaque ID accepted");
        }
        request.inventory[0].native_session_id = "../../opaque\\provider-id".into();
        request.validate().unwrap();
        let mut duplicate = request.inventory[0].clone();
        duplicate.runtime_relative_path = "claude/session-runtime-2".into();
        request.inventory.insert(1, duplicate);
        assert!(
            request.validate().is_err(),
            "two runtimes cannot claim one provider alias"
        );
        for path in [
            "relative/audit",
            "/audit/../source",
            "/audit//source",
            "/audit/source/",
            "/",
        ] {
            let mut request = fixture();
            request.destination.audit_path = path.into();
            assert!(
                request.validate().is_err(),
                "nonliteral audit path accepted"
            );
        }
    }

    #[test]
    fn unknown_duplicate_and_wrong_typed_fields_are_not_wire_compatible() {
        let request = fixture();
        for path in [
            "",
            "/destination",
            "/destination/owner",
            "/inventory/0",
            "/inventory/0/creation_owner",
            "/source_owners/0/owner",
        ] {
            let mut value = serde_json::to_value(&request).unwrap();
            value
                .pointer_mut(path)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .insert("unknown".into(), Value::Bool(true));
            assert!(
                serde_json::from_value::<TransferRequest>(value).is_err(),
                "unknown field at {path} accepted"
            );
        }
        let raw = serde_json::to_string(&request).unwrap().replacen(
            "\"version\":1",
            "\"version\":1,\"version\":1",
            1,
        );
        assert!(serde_json::from_str::<TransferRequest>(&raw).is_err());
        for path in [
            "",
            "/destination",
            "/destination/owner",
            "/mappings/0",
            "/mappings/0/effective_owner",
            "/source_closures/0",
            "/source_closures/0/owner",
            "/source_seals/0",
        ] {
            let mut value = serde_json::to_value(receipt(&request)).unwrap();
            value
                .pointer_mut(path)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .insert("unknown".into(), Value::Bool(true));
            assert!(
                serde_json::from_value::<TransferReceipt>(value).is_err(),
                "unknown receipt field at {path} accepted"
            );
        }
    }
}
