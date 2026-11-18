//! Identity for supervised publication commands, separate from native VM ownership.
//!
//! This is controller data, not dispatch authorization or a cleanup capability.
//! Only a protected supervisor can produce the existing quiescence proof.
use crate::native_vm::RuntimeOwner;
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Role {
    Publication,
}

/// Versioned publication-command identity. Construction and deserialization
/// reject unsupported versions, roles, and invalid workflow/lease identities.
/// Its identity conversion grants neither a native runtime nor a model call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "WireOwner")]
pub struct PublicationCommandOwner {
    schema_version: u32,
    role: Role,
    workflow_id: String,
    lease_generation: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireOwner {
    schema_version: u32,
    role: Role,
    workflow_id: String,
    lease_generation: i64,
}

impl TryFrom<WireOwner> for PublicationCommandOwner {
    type Error = anyhow::Error;

    fn try_from(wire: WireOwner) -> Result<Self> {
        ensure!(
            wire.schema_version == 1,
            "unsupported publication custody version"
        );
        let Role::Publication = wire.role;
        Self::new(wire.workflow_id, wire.lease_generation)
    }
}

impl PublicationCommandOwner {
    pub fn new(workflow_id: String, lease_generation: i64) -> Result<Self> {
        ensure!(
            !workflow_id.is_empty(),
            "publication custody workflow missing"
        );
        ensure!(
            lease_generation > 0,
            "publication custody lease generation invalid"
        );
        Ok(Self {
            schema_version: 1,
            role: Role::Publication,
            workflow_id,
            lease_generation,
        })
    }

    pub fn workflow_id(&self) -> &str {
        &self.workflow_id
    }

    pub fn lease_generation(&self) -> i64 {
        self.lease_generation
    }

    /// The existing supervisor proof uses this workflow/lease pair. Converting
    /// identity is not permission to register a native VM owner.
    pub fn supervision_identity(&self) -> RuntimeOwner {
        RuntimeOwner {
            workflow_id: self.workflow_id.clone(),
            lease_generation: self.lease_generation,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn publication_identity_has_an_explicit_version_and_role() {
        let owner = PublicationCommandOwner::new("owner/repo#7".into(), 4).unwrap();
        let wire = json!({"schema_version":1,"role":"publication","workflow_id":"owner/repo#7","lease_generation":4});
        assert_eq!(serde_json::to_value(&owner).unwrap(), wire);
        assert_eq!(
            serde_json::from_value::<PublicationCommandOwner>(wire).unwrap(),
            owner
        );
        let identity = owner.supervision_identity();
        assert_eq!(identity.workflow_id, owner.workflow_id());
        assert_eq!(identity.lease_generation, owner.lease_generation());
    }

    #[test]
    fn unsupported_roles_versions_and_authority_fields_are_refused() {
        let valid = json!({"schema_version":1,"role":"publication","workflow_id":"owner/repo#7","lease_generation":4});
        for (field, value) in [
            ("schema_version", json!(0)),
            ("schema_version", json!(2)),
            ("role", json!("native")),
            ("role", json!("verifier_recovery")),
            ("native_runtime_owner", json!(true)),
            ("model_calls_allowed", json!(true)),
            ("workflow_id", json!("")),
            ("lease_generation", json!(0)),
            ("lease_generation", json!(-1)),
        ] {
            let mut wire = valid.clone();
            wire[field] = value;
            assert!(
                serde_json::from_value::<PublicationCommandOwner>(wire).is_err(),
                "accepted invalid {field}"
            );
        }
        for field in ["schema_version", "role", "workflow_id", "lease_generation"] {
            let mut wire = valid.clone();
            wire.as_object_mut().unwrap().remove(field);
            assert!(
                serde_json::from_value::<PublicationCommandOwner>(wire).is_err(),
                "accepted missing {field}"
            );
        }
        assert!(PublicationCommandOwner::new(String::new(), 1).is_err());
        assert!(PublicationCommandOwner::new("owner/repo#7".into(), 0).is_err());
    }
}
