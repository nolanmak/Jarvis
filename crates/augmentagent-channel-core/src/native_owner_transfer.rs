//! Controller-owned runtime handoff; creation receipts remain immutable.
pub use super::owner_recovery::{ClosedWorkflowPlan,ClosedWorkflowProof,with_closed_workflow};
pub use super::owner_outcome::{
    capture_owner_binding, capture_transfer_binding, transfer, transfer_receipt,
    validate_owner_binding, with_initial_owner_binding, TransferBinding, TransferFenceError,
    TransferObservation, TransferOutcome, TransferReason,
};
pub use super::owner_preparation::{NoPendingPreparationFailure,PreparationFingerprintInput,PreparationCapacity,PreparationRuntime,PreparationPlan,PreparedHandoffIntent,PreparationPersistence,PreparationScan,PreparedTransferResult,prepare_transfer};
pub use super::owner_reconciliation::{with_transfer_inspection,NativeTransferInspection,NativeTransferState,with_retired_aborted_namespace,RetiredNativeNamespace};
use anyhow::{ensure, Result};
#[cfg(test)]
use serde_json::Value;

pub use super::owner_completion::{
    observe_native_completion, observe_native_uncertainty, VerifiedNativeObservation,
};

pub use crate::native_owner_transfer_wire::{
    canonical_bytes, EffectiveMapping as RuntimeMapping, InventoryRow as RuntimeInventoryRow,
    OwnerAudit as Authority, Provider, SourceClosure, SourceSeal as SealReference, TransferReceipt,
    TransferRequest,
};

impl TransferReceipt {
    pub(crate) fn for_request(
        request: &TransferRequest,
        seals: Vec<SealReference>,
    ) -> Result<Self> {
        use sha2::{Digest, Sha256};
        request.validate()?;
        let mut paths: Vec<_> = request
            .source_owners
            .iter()
            .map(|source| source.audit_path.clone())
            .collect();
        paths.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        paths.dedup();
        ensure!(
            seals.iter().map(|seal| &seal.audit_path).eq(paths.iter()),
            "native-transfer-audit-corrupt"
        );
        ensure!(
            seals.iter().all(|seal| seal.seal_sha256.len() == 64
                && seal
                    .seal_sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))),
            "native-transfer-audit-corrupt"
        );
        let mut receipt = Self {
            version: 1,
            transfer_id: request.transfer_id.clone(),
            request_sha256: request.request_digest()?,
            source_inventory_sha256: request.inventory_digest()?,
            destination: request.destination.clone(),
            source_closures: request
                .source_owners
                .iter()
                .map(|source| SourceClosure {
                    owner: source.owner.clone(),
                    audit_path: source.audit_path.clone(),
                    closed: true,
                })
                .collect(),
            source_seals: seals,
            mappings: request
                .inventory
                .iter()
                .map(|row| RuntimeMapping {
                    provider: row.provider,
                    native_session_sha256: format!(
                        "{:x}",
                        Sha256::digest(row.native_session_id.as_bytes())
                    ),
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
        receipt.receipt_sha256 = crate::native_owner_transfer_wire::receipt_digest(&receipt)?;
        receipt.verify(request)?;
        Ok(receipt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn handoff_wire_has_one_exact_encoding() {
        let value: Value =
            serde_json::from_str(r#"{"z":"é/\\\"\b\u0001","a":[{"b":2,"a":1}]}"#).unwrap();
        assert_eq!(
            canonical_bytes(&value).unwrap(),
            "{\"a\":[{\"a\":1,\"b\":2}],\"z\":\"é/\\\\\\\"\\b\\u0001\"}".as_bytes()
        );
    }
    #[test]
    fn handoff_wire_refuses_floats_in_any_position() {
        for value in [
            serde_json::json!(1.5),
            serde_json::json!({"nested":[1,1.0]}),
            serde_json::json!(-0.0),
        ] {
            assert!(
                canonical_bytes(&value).is_err(),
                "handoff accepted an ambiguous floating-point encoding: {value}"
            );
        }
    }
    fn request_fixture() -> Value {
        let source = serde_json::json!({"workflow_id":"workflow","lease_generation":1});
        serde_json::json!({"version":1,"transfer_id":"123e4567-e89b-42d3-a456-426614174000",
            "workflow_id":"workflow","source_owners":[{"owner":source,"audit_path":"/audit/source.jsonl"}],
            "destination":{"owner":{"workflow_id":"workflow","lease_generation":2},"audit_path":"/audit/destination.jsonl"},
            "inventory":[{"provider":"codex","native_session_id":"native-one","runtime_relative_path":"codex/session-one",
                "creation_owner":source,"creation_audit_path":"/audit/source.jsonl",
                "effective_owner":source,"effective_audit_path":"/audit/source.jsonl",
                "creation_receipt_sha256":"a".repeat(64),"completion_sha256":"b".repeat(64),
                "alias_sha256":"c".repeat(64),"export_manifest_sha256":"d".repeat(64)}]})
    }
    #[test]
    fn native_transfer_request_binds_every_inventory_field() {
        let value = request_fixture();
        let request = TransferRequest::parse(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(
            request.request_digest().unwrap(),
            "0cac6f42ffc523bc060bf1bc1994586411fda451143d5cb6fe3154bf3998f90b"
        );
        assert_eq!(
            request.inventory_digest().unwrap(),
            "b242f588d275bbda74741f459d95b729efe429e2c5a0e1c90c57979ade2e2a5f"
        );
        let mut changed = value.clone();
        changed["inventory"][0]["native_session_id"] = serde_json::json!("another-session");
        let changed = TransferRequest::parse(&serde_json::to_vec(&changed).unwrap()).unwrap();
        assert_ne!(
            request.request_digest().unwrap(),
            changed.request_digest().unwrap()
        );
        assert_ne!(
            request.inventory_digest().unwrap(),
            changed.inventory_digest().unwrap()
        );
        let mut changed = value;
        changed["transfer_id"] = serde_json::json!("223e4567-e89b-42d3-a456-426614174000");
        let changed = TransferRequest::parse(&serde_json::to_vec(&changed).unwrap()).unwrap();
        assert_ne!(
            request.request_digest().unwrap(),
            changed.request_digest().unwrap()
        );
        assert_eq!(
            request.inventory_digest().unwrap(),
            changed.inventory_digest().unwrap()
        );
    }
    #[test]
    fn native_transfer_request_refuses_ambiguous_or_disconnected_authority() {
        let base = request_fixture();
        let mut cases = Vec::new();
        for (pointer, replacement) in [
            ("/version", serde_json::json!(2)),
            (
                "/transfer_id",
                serde_json::json!("123E4567-E89B-42D3-A456-426614174000"),
            ),
            ("/destination/owner/lease_generation", serde_json::json!(1)),
            (
                "/destination/owner/workflow_id",
                serde_json::json!("other-workflow"),
            ),
            (
                "/destination/audit_path",
                serde_json::json!("/audit/source.jsonl"),
            ),
            ("/inventory/0/provider", serde_json::json!("router")),
            ("/inventory/0/native_session_id", serde_json::json!(" ")),
            (
                "/inventory/0/native_session_id",
                serde_json::json!("x".repeat(257)),
            ),
            (
                "/inventory/0/runtime_relative_path",
                serde_json::json!("codex/../session-one"),
            ),
            (
                "/inventory/0/runtime_relative_path",
                serde_json::json!("claude/session-one"),
            ),
            (
                "/inventory/0/creation_receipt_sha256",
                serde_json::json!("A".repeat(64)),
            ),
            (
                "/inventory/0/effective_owner/lease_generation",
                serde_json::json!(0),
            ),
            (
                "/inventory/0/effective_audit_path",
                serde_json::json!("/audit/foreign.jsonl"),
            ),
        ] {
            let mut value = base.clone();
            *value.pointer_mut(pointer).unwrap() = replacement;
            cases.push((pointer, value));
        }
        let mut duplicate = base.clone();
        duplicate["inventory"]
            .as_array_mut()
            .unwrap()
            .push(base["inventory"][0].clone());
        cases.push(("duplicate runtime", duplicate));
        let mut duplicate = base.clone();
        duplicate["source_owners"]
            .as_array_mut()
            .unwrap()
            .push(base["source_owners"][0].clone());
        cases.push(("duplicate source", duplicate));
        for (name, value) in cases {
            assert!(
                TransferRequest::parse(&serde_json::to_vec(&value).unwrap()).is_err(),
                "accepted {name}"
            );
        }
    }
    #[test]
    fn native_transfer_request_rejects_unknown_fields_at_all_levels() {
        for pointer in [
            "",
            "/source_owners/0",
            "/source_owners/0/owner",
            "/destination",
            "/destination/owner",
            "/inventory/0",
            "/inventory/0/creation_owner",
            "/inventory/0/effective_owner",
        ] {
            let mut value = request_fixture();
            value
                .pointer_mut(pointer)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .insert("unknown".into(), Value::Null);
            assert!(
                TransferRequest::parse(&serde_json::to_vec(&value).unwrap()).is_err(),
                "accepted unknown field at {pointer}"
            );
        }
    }

    #[test]
    fn native_transfer_multi_provider_and_multi_owner_inventory_has_one_order() {
        let mut value = request_fixture();
        let second = serde_json::json!({"owner":{"workflow_id":"workflow","lease_generation":2},"audit_path":"/audit/second.jsonl"});
        value["source_owners"]
            .as_array_mut()
            .unwrap()
            .push(second.clone());
        value["destination"]["owner"]["lease_generation"] = serde_json::json!(3);
        let mut claude = value["inventory"][0].clone();
        claude["provider"] = serde_json::json!("claude");
        claude["runtime_relative_path"] = serde_json::json!("claude/session-two");
        claude["effective_owner"] = second["owner"].clone();
        claude["effective_audit_path"] = second["audit_path"].clone();
        value["inventory"].as_array_mut().unwrap().insert(0, claude);
        TransferRequest::parse(&serde_json::to_vec(&value).unwrap()).unwrap();
        for field in ["inventory", "source_owners"] {
            let mut reversed = value.clone();
            reversed[field].as_array_mut().unwrap().reverse();
            assert!(
                TransferRequest::parse(&serde_json::to_vec(&reversed).unwrap()).is_err(),
                "accepted reversed {field}"
            );
        }
    }
    #[test]
    fn native_transfer_source_limit_refuses_overflow_without_truncating() {
        let mut value = request_fixture();
        value["inventory"] = serde_json::json!([]);
        value["destination"]["owner"]["lease_generation"] = serde_json::json!(66);
        value["source_owners"] = Value::Array(
            (1..=64)
                .map(|generation| {
                    serde_json::json!({
            "owner":{"workflow_id":"workflow","lease_generation":generation},
            "audit_path":format!("/audit/source-{generation}.jsonl")})
                })
                .collect(),
        );
        let mut request = TransferRequest::parse(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(request.source_owners.len(), 64);
        request.source_owners.push(Authority {
            owner: super::super::RuntimeOwner {
                workflow_id: "workflow".into(),
                lease_generation: 65,
            },
            audit_path: "/audit/source-65.jsonl".into(),
        });
        assert!(request.request_digest().is_err());
        assert!(TransferRequest::parse(&serde_json::to_vec(&request).unwrap()).is_err());
        assert_eq!(request.source_owners.len(), 65);
    }

    #[test]
    fn live_transfer_wire_refuses_zero_generation_before_binding_hashes() {
        let mut value = request_fixture();
        for pointer in [
            "/source_owners/0/owner/lease_generation",
            "/inventory/0/creation_owner/lease_generation",
            "/inventory/0/effective_owner/lease_generation",
        ] {
            *value.pointer_mut(pointer).unwrap() = serde_json::json!(0);
        }
        let strict: crate::native_owner_transfer_wire::TransferRequest =
            serde_json::from_value(value.clone()).unwrap();
        assert!(strict.validate().is_err());
        assert!(
            TransferRequest::parse(&serde_json::to_vec(&value).unwrap()).is_err(),
            "live transfer accepted an owner the shared wire contract rejects"
        );
    }

    #[test]
    fn live_transfer_wire_accepts_distinct_ordered_owner_audit_pairs() {
        let mut value = request_fixture();
        value["inventory"] = serde_json::json!([]);
        let mut second = value["source_owners"][0].clone();
        second["audit_path"] = serde_json::json!("/audit/z-other.jsonl");
        value["source_owners"].as_array_mut().unwrap().push(second);
        let strict: crate::native_owner_transfer_wire::TransferRequest =
            serde_json::from_value(value.clone()).unwrap();
        strict.validate().unwrap();
        let live = TransferRequest::parse(&serde_json::to_vec(&value).unwrap())
            .expect("live transfer rejected a complete distinct owner/audit pair");
        assert_eq!(
            live.request_digest().unwrap(),
            crate::native_owner_transfer_wire::request_digest(&strict).unwrap()
        );
    }
}
