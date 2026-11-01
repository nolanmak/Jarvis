//! Controller-owned runtime handoff; creation receipts remain immutable.
use anyhow::{Result,ensure};
use serde_json::Value;

pub fn canonical_bytes(value:&Value)->Result<Vec<u8>> {
    fn append(value:&Value,bytes:&mut Vec<u8>)->Result<()> {
        match value {
            Value::Null=>bytes.extend_from_slice(b"null"),
            Value::Bool(value)=>bytes.extend_from_slice(if *value {b"true"} else {b"false"}),
            Value::Number(value)=>{
                ensure!(value.is_i64() || value.is_u64(),"handoff JSON requires integer numbers");
                bytes.extend_from_slice(value.to_string().as_bytes());
            },
            Value::String(value)=>bytes.extend(serde_json::to_vec(value)?),
            Value::Array(values)=>{
                bytes.push(b'[');
                for (index,value) in values.iter().enumerate() {
                    if index>0 {bytes.push(b',');}
                    append(value,bytes)?;
                }
                bytes.push(b']');
            },
            Value::Object(values)=>{
                bytes.push(b'{');
                let mut entries:Vec<_>=values.iter().collect();
                entries.sort_by(|a,b|a.0.as_bytes().cmp(b.0.as_bytes()));
                for (index,(key,value)) in entries.into_iter().enumerate() {
                    if index>0 {bytes.push(b',');}
                    bytes.extend(serde_json::to_vec(key)?);bytes.push(b':');append(value,bytes)?;
                }
                bytes.push(b'}');
            },
        }
        Ok(())
    }
    let mut bytes=Vec::new();append(value,&mut bytes)?;Ok(bytes)
}

/// Immutable wire authority; current dispatch eligibility is resolved in the ledger.
#[derive(Debug,Clone,PartialEq,Eq,serde::Serialize,serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Authority {
    pub owner:super::RuntimeOwner,
    pub audit_path:String,
}

#[derive(Debug,Clone,PartialEq,Eq,serde::Serialize,serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeInventoryRow {
    pub provider:String,
    pub native_session_id:String,
    pub runtime_relative_path:String,
    pub creation_owner:super::RuntimeOwner,
    pub creation_audit_path:String,
    pub effective_owner:super::RuntimeOwner,
    pub effective_audit_path:String,
    pub creation_receipt_sha256:String,
    pub completion_sha256:String,
    pub alias_sha256:String,
    pub export_manifest_sha256:String,
}

#[derive(Debug,Clone,PartialEq,Eq,serde::Serialize,serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferRequest {
    pub version:u32,
    pub transfer_id:String,
    pub workflow_id:String,
    pub source_owners:Vec<Authority>,
    pub destination:Authority,
    pub inventory:Vec<RuntimeInventoryRow>,
}
impl TransferRequest {
    pub fn parse(bytes:&[u8])->Result<Self> {
        let request:Self=serde_json::from_slice(bytes)?;
        request.validate()?;Ok(request)
    }
    pub fn validate(&self)->Result<()> {Ok(())}
    pub fn request_digest(&self)->Result<String> {
        self.validate()?;hash_value(&serde_json::to_value(self)?)
    }
    pub fn inventory_digest(&self)->Result<String> {
        self.validate()?;hash_value(&serde_json::json!({"version":1,"inventory":self.inventory}))
    }
}
fn hash_value(value:&Value)->Result<String> {
    use sha2::{Digest,Sha256};
    Ok(format!("{:x}",Sha256::digest(canonical_bytes(value)?)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn handoff_wire_has_one_exact_encoding() {
        let value:Value=serde_json::from_str(r#"{"z":"é/\\\"\b\u0001","a":[{"b":2,"a":1}]}"#).unwrap();
        assert_eq!(canonical_bytes(&value).unwrap(),
            "{\"a\":[{\"a\":1,\"b\":2}],\"z\":\"é/\\\\\\\"\\b\\u0001\"}".as_bytes());
    }
    #[test]
    fn handoff_wire_refuses_floats_in_any_position() {
        for value in [serde_json::json!(1.5),serde_json::json!({"nested":[1,1.0]}),serde_json::json!(-0.0)] {
            assert!(canonical_bytes(&value).is_err(),"handoff accepted an ambiguous floating-point encoding: {value}");
        }
    }
    fn request_fixture()->Value {
        let source=serde_json::json!({"workflow_id":"workflow","lease_generation":1});
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
        let value=request_fixture();let request=TransferRequest::parse(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(request.request_digest().unwrap(),hash_value(&value).unwrap());
        let mut changed=value.clone();changed["inventory"][0]["native_session_id"]=serde_json::json!("another-session");
        let changed=TransferRequest::parse(&serde_json::to_vec(&changed).unwrap()).unwrap();
        assert_ne!(request.request_digest().unwrap(),changed.request_digest().unwrap());
        assert_ne!(request.inventory_digest().unwrap(),changed.inventory_digest().unwrap());
        let mut changed=value;changed["transfer_id"]=serde_json::json!("223e4567-e89b-42d3-a456-426614174000");
        let changed=TransferRequest::parse(&serde_json::to_vec(&changed).unwrap()).unwrap();
        assert_ne!(request.request_digest().unwrap(),changed.request_digest().unwrap());
        assert_eq!(request.inventory_digest().unwrap(),changed.inventory_digest().unwrap());
    }
    #[test]
    fn native_transfer_request_refuses_ambiguous_or_disconnected_authority() {
        let base=request_fixture();
        let mut cases=Vec::new();
        for (pointer,replacement) in [
            ("/version",serde_json::json!(2)),
            ("/transfer_id",serde_json::json!("123E4567-E89B-42D3-A456-426614174000")),
            ("/destination/owner/lease_generation",serde_json::json!(1)),
            ("/destination/owner/workflow_id",serde_json::json!("other-workflow")),
            ("/destination/audit_path",serde_json::json!("/audit/source.jsonl")),
            ("/inventory/0/provider",serde_json::json!("router")),
            ("/inventory/0/native_session_id",serde_json::json!(" ")),
            ("/inventory/0/native_session_id",serde_json::json!("x".repeat(257))),
            ("/inventory/0/runtime_relative_path",serde_json::json!("codex/../session-one")),
            ("/inventory/0/runtime_relative_path",serde_json::json!("claude/session-one")),
            ("/inventory/0/creation_receipt_sha256",serde_json::json!("A".repeat(64))),
            ("/inventory/0/effective_owner/lease_generation",serde_json::json!(0)),
            ("/inventory/0/effective_audit_path",serde_json::json!("/audit/foreign.jsonl")),
        ] {let mut value=base.clone();*value.pointer_mut(pointer).unwrap()=replacement;cases.push((pointer,value));}
        let mut duplicate=base.clone();duplicate["inventory"].as_array_mut().unwrap().push(base["inventory"][0].clone());cases.push(("duplicate runtime",duplicate));
        let mut duplicate=base.clone();duplicate["source_owners"].as_array_mut().unwrap().push(base["source_owners"][0].clone());cases.push(("duplicate source",duplicate));
        for (name,value) in cases {
            assert!(TransferRequest::parse(&serde_json::to_vec(&value).unwrap()).is_err(),"accepted {name}");
        }
    }
    #[test]
    fn native_transfer_request_rejects_unknown_fields_at_all_levels() {
        for pointer in ["","/source_owners/0","/source_owners/0/owner","/destination","/destination/owner","/inventory/0","/inventory/0/creation_owner","/inventory/0/effective_owner"] {
            let mut value=request_fixture();value.pointer_mut(pointer).unwrap().as_object_mut().unwrap().insert("unknown".into(),Value::Null);
            assert!(TransferRequest::parse(&serde_json::to_vec(&value).unwrap()).is_err(),"accepted unknown field at {pointer}");
        }
    }

}
