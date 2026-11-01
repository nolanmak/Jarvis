//! Controller-owned runtime handoff; creation receipts remain immutable.
use anyhow::{Result,ensure};
use serde_json::Value;
pub use super::owner_outcome::{capture_owner_binding, with_initial_owner_binding, validate_owner_binding, capture_transfer_binding, transfer, transfer_receipt, TransferBinding, TransferFenceError, TransferObservation, TransferOutcome, TransferReason};

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
    /// Checks the wire contract only. Filesystem inventory, anchored audits,
    /// controller fencing and current ownership still require the transfer ledger.
    pub fn validate(&self)->Result<()> {
        use std::collections::HashSet;
        ensure!(self.version==1,"unsupported native transfer request version");
        ensure!(uuid::Uuid::parse_str(&self.transfer_id)?.to_string()==self.transfer_id,"noncanonical native transfer UUID");
        ensure!(!self.workflow_id.is_empty(),"empty native transfer workflow");
        ensure!(!self.source_owners.is_empty() && self.source_owners.len()<=64 && self.inventory.len()<=64,"native transfer inventory bound");
        let authority=|owner:&super::RuntimeOwner,path:&str|->Result<()> {
            ensure!(owner.workflow_id==self.workflow_id && owner.lease_generation>=0,"native transfer workflow mismatch");
            ensure!(std::path::Path::new(path).is_absolute() && !path.contains('\0'),"invalid native audit path");Ok(())
        };
        authority(&self.destination.owner,&self.destination.audit_path)?;
        let mut generations=HashSet::new();
        for source in &self.source_owners {
            authority(&source.owner,&source.audit_path)?;
            ensure!(source.owner.lease_generation<self.destination.owner.lease_generation,"native transfer destination is not newer");
            ensure!(source.audit_path!=self.destination.audit_path,"native transfer audit namespace reused");
            ensure!(generations.insert(source.owner.lease_generation),"duplicate native source owner");
        }
        ensure!(self.source_owners.windows(2).all(|pair|
            (pair[0].owner.lease_generation,pair[0].audit_path.as_bytes()) <
            (pair[1].owner.lease_generation,pair[1].audit_path.as_bytes())),"native source owners are not ordered");
        let mut runtimes=HashSet::new();let mut sessions=HashSet::new();
        for row in &self.inventory {
            ensure!(matches!(row.provider.as_str(),"codex"|"claude"),"invalid native transfer provider");
            ensure!(!row.native_session_id.is_empty() && row.native_session_id.len()<=256 &&
                row.native_session_id.bytes().all(|byte|(33..=126).contains(&byte)),"invalid observed native session ID");
            let parts:Vec<_>=row.runtime_relative_path.split('/').collect();
            ensure!(parts.len()==2 && parts[0]==row.provider && parts[1].starts_with("session-") && parts[1].len()>8 &&
                !parts.iter().any(|part|part.is_empty() || *part=="." || *part==".." || part.contains('\0')),
                "invalid native runtime relative path");
            ensure!(runtimes.insert(row.runtime_relative_path.as_str()) && sessions.insert((row.provider.as_str(),row.native_session_id.as_str())),"duplicate native runtime or session");
            authority(&row.creation_owner,&row.creation_audit_path)?;authority(&row.effective_owner,&row.effective_audit_path)?;
            ensure!(row.creation_owner.lease_generation<=row.effective_owner.lease_generation,"native creation provenance is newer than effective owner");
            ensure!(row.creation_owner!=row.effective_owner || row.creation_audit_path==row.effective_audit_path,"native creation authority has conflicting audit paths");
            ensure!(self.source_owners.iter().any(|source|source.owner==row.effective_owner && source.audit_path==row.effective_audit_path),"native runtime is disconnected from source authority");
            for hash in [&row.creation_receipt_sha256,&row.completion_sha256,&row.alias_sha256,&row.export_manifest_sha256] {
                ensure!(hash.len()==64 && hash.bytes().all(|byte|byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),"invalid native transfer digest");
            }
        }
        ensure!(self.inventory.windows(2).all(|pair|
            (pair[0].provider.as_bytes(),pair[0].runtime_relative_path.as_bytes()) <
            (pair[1].provider.as_bytes(),pair[1].runtime_relative_path.as_bytes())),"native transfer inventory is not ordered");
        Ok(())
    }
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

#[derive(Debug,Clone,PartialEq,Eq,serde::Serialize,serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceClosure {
    pub owner:super::RuntimeOwner,
    pub audit_path:String,
    pub closed:bool,
}
#[derive(Debug,Clone,PartialEq,Eq,serde::Serialize,serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SealReference {pub audit_path:String,pub seal_sha256:String}
#[derive(Debug,Clone,PartialEq,Eq,serde::Serialize,serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeMapping {
    pub provider:String,
    pub native_session_sha256:String,
    pub runtime_relative_path:String,
    pub creation_receipt_sha256:String,
    pub completion_sha256:String,
    pub export_manifest_sha256:String,
    pub effective_owner:super::RuntimeOwner,
    pub effective_audit_path:String,
    pub transfer_id:String,
}
#[derive(Debug,Clone,PartialEq,Eq,serde::Serialize,serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferReceipt {
    pub version:u32,
    pub transfer_id:String,
    pub request_sha256:String,
    pub source_inventory_sha256:String,
    pub source_closures:Vec<SourceClosure>,
    pub source_seals:Vec<SealReference>,
    pub destination:Authority,
    pub mappings:Vec<RuntimeMapping>,
    pub receipt_sha256:String,
}
impl TransferReceipt {
    pub(crate) fn for_request(request:&TransferRequest,seals:Vec<SealReference>)->Result<Self> {
        use sha2::{Digest,Sha256};
        request.validate()?;
        let mut paths:Vec<_>=request.source_owners.iter().map(|source|source.audit_path.clone()).collect();
        paths.sort_by(|a,b|a.as_bytes().cmp(b.as_bytes()));paths.dedup();
        ensure!(seals.iter().map(|seal|&seal.audit_path).eq(paths.iter()),"native-transfer-audit-corrupt");
        ensure!(seals.iter().all(|seal|seal.seal_sha256.len()==64 && seal.seal_sha256.bytes().all(|byte|byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))),"native-transfer-audit-corrupt");
        let mut receipt=Self{version:1,transfer_id:request.transfer_id.clone(),request_sha256:request.request_digest()?,
            source_inventory_sha256:request.inventory_digest()?,destination:request.destination.clone(),
            source_closures:request.source_owners.iter().map(|source|SourceClosure{owner:source.owner.clone(),audit_path:source.audit_path.clone(),closed:true}).collect(),
            source_seals:seals,mappings:request.inventory.iter().map(|row|RuntimeMapping{provider:row.provider.clone(),
                native_session_sha256:format!("{:x}",Sha256::digest(row.native_session_id.as_bytes())),runtime_relative_path:row.runtime_relative_path.clone(),
                creation_receipt_sha256:row.creation_receipt_sha256.clone(),completion_sha256:row.completion_sha256.clone(),export_manifest_sha256:row.export_manifest_sha256.clone(),
                effective_owner:request.destination.owner.clone(),effective_audit_path:request.destination.audit_path.clone(),transfer_id:request.transfer_id.clone()}).collect(),receipt_sha256:String::new()};
        let mut value=serde_json::to_value(&receipt)?;value.as_object_mut().expect("receipt object").remove("receipt_sha256");
        receipt.receipt_sha256=hash_value(&value)?;Ok(receipt)
    }
    /// Historical receipt binding only; dispatch additionally needs current ledger authority.
    pub fn verify(&self,request:&TransferRequest)->Result<()> {
        ensure!(*self==Self::for_request(request,self.source_seals.clone())?,"native-transfer-prepared-corrupt");Ok(())
    }
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
        assert_eq!(request.request_digest().unwrap(),"0cac6f42ffc523bc060bf1bc1994586411fda451143d5cb6fe3154bf3998f90b");
        assert_eq!(request.inventory_digest().unwrap(),"b242f588d275bbda74741f459d95b729efe429e2c5a0e1c90c57979ade2e2a5f");
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

    #[test]
    fn native_transfer_multi_provider_and_multi_owner_inventory_has_one_order() {
        let mut value=request_fixture();
        let second=serde_json::json!({"owner":{"workflow_id":"workflow","lease_generation":2},"audit_path":"/audit/second.jsonl"});
        value["source_owners"].as_array_mut().unwrap().push(second.clone());
        value["destination"]["owner"]["lease_generation"]=serde_json::json!(3);
        let mut claude=value["inventory"][0].clone();claude["provider"]=serde_json::json!("claude");
        claude["runtime_relative_path"]=serde_json::json!("claude/session-two");
        claude["effective_owner"]=second["owner"].clone();claude["effective_audit_path"]=second["audit_path"].clone();
        value["inventory"].as_array_mut().unwrap().insert(0,claude);
        TransferRequest::parse(&serde_json::to_vec(&value).unwrap()).unwrap();
        for field in ["inventory","source_owners"] {
            let mut reversed=value.clone();reversed[field].as_array_mut().unwrap().reverse();
            assert!(TransferRequest::parse(&serde_json::to_vec(&reversed).unwrap()).is_err(),"accepted reversed {field}");
        }
    }
    #[test]
    fn native_transfer_source_limit_refuses_overflow_without_truncating() {
        let mut value=request_fixture();value["inventory"]=serde_json::json!([]);
        value["destination"]["owner"]["lease_generation"]=serde_json::json!(65);
        value["source_owners"]=Value::Array((0..64).map(|generation|serde_json::json!({
            "owner":{"workflow_id":"workflow","lease_generation":generation},
            "audit_path":format!("/audit/source-{generation}.jsonl")})).collect());
        let mut request=TransferRequest::parse(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(request.source_owners.len(),64);
        request.source_owners.push(Authority{owner:super::super::RuntimeOwner{workflow_id:"workflow".into(),lease_generation:64},audit_path:"/audit/source-64.jsonl".into()});
        assert!(request.request_digest().is_err());
        assert!(TransferRequest::parse(&serde_json::to_vec(&request).unwrap()).is_err());
        assert_eq!(request.source_owners.len(),65);
    }

}
