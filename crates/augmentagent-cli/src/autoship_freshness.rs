//! Bounded C/S prerequisite protocol. A hold never grants CI or merge authority.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Reason {
    LegacyFormat,
    MetadataMissing,
    MetadataTorn,
    GenerationMismatch,
    InputCensusUnknown,
    ArtifactProvenanceUnknown,
    AuthorityChanged,
    InvalidationUnsafe,
    CleanupUncertain,
    FreshnessUnknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Recovery {
    RevalidateInputs,
    RebuildSourceArtifacts,
    RestoreTrustedDependencies,
    AwaitVerifiedCleanup,
    AwaitCompatibleController,
}

impl Reason {
    pub(crate) fn recovery(self) -> Recovery {
        match self {
            Self::LegacyFormat | Self::AuthorityChanged | Self::FreshnessUnknown => Recovery::AwaitCompatibleController,
            Self::MetadataMissing | Self::MetadataTorn | Self::GenerationMismatch => Recovery::RebuildSourceArtifacts,
            Self::InputCensusUnknown => Recovery::RevalidateInputs,
            Self::ArtifactProvenanceUnknown => Recovery::RestoreTrustedDependencies,
            Self::InvalidationUnsafe | Self::CleanupUncertain => Recovery::AwaitVerifiedCleanup,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Hold {
    version: u32,
    pub(crate) reason: Reason,
    pub(crate) recovery: Recovery,
    pub(crate) evidence_sha256: String,
}

impl Hold {
    pub(crate) fn parse(value: &serde_json::Value) -> Self {
        if let Ok(hold) = serde_json::from_value::<Self>(value.clone()) {
            if hold.version == 1 && hold.recovery == hold.reason.recovery()
                && hold.evidence_sha256.len() == 64
                && hold.evidence_sha256.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')) {
                return hold;
            }
        }
        Self {
            version: 1, reason: Reason::FreshnessUnknown,
            recovery: Recovery::AwaitCompatibleController,
            evidence_sha256: format!("{:x}", Sha256::digest(value.to_string().as_bytes())),
        }
    }
}

impl std::fmt::Display for Hold {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "shared CI prerequisite unavailable: freshness {:?}; recovery {:?}; evidence {}",
            self.reason, self.recovery, self.evidence_sha256)
    }
}
impl std::error::Error for Hold {}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn actual_python_producer_roundtrips_every_hold_family() {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let mut child = Command::new("/usr/bin/python3").args(["-I", "-c",
            "import json,sys; scope={}; exec(compile(sys.stdin.buffer.read(),'<compiled freshness controller>','exec'),scope); json.dump([scope['FreshnessHold'](reason,b'fixture').diagnostic() for reason in scope['RECOVERY']],sys.stdout)"])
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        child.stdin.take().unwrap().write_all(include_bytes!("../../../scripts/ci-cache-generation.py")).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        let values: Vec<serde_json::Value> = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(values.len(), 10);
        for value in values {
            assert_eq!(serde_json::to_value(Hold::parse(&value)).unwrap(), value,
                "producer/consumer freshness protocol drift");
        }
    }

    #[test]
    fn codes_roundtrip_and_invalid_protocol_becomes_unknown_without_raw_content() {
        for reason in [Reason::LegacyFormat,Reason::MetadataMissing,Reason::MetadataTorn,Reason::GenerationMismatch,
            Reason::InputCensusUnknown,Reason::ArtifactProvenanceUnknown,Reason::AuthorityChanged,
            Reason::InvalidationUnsafe,Reason::CleanupUncertain,Reason::FreshnessUnknown] {
            let value = serde_json::json!({"version":1,"reason":reason,"recovery":reason.recovery(),"evidence_sha256":"a".repeat(64)});
            assert_eq!(Hold::parse(&value).reason, reason);
            for (key, replacement) in [("version",serde_json::json!(999)),("reason",serde_json::json!("SECRET")),
                ("recovery",serde_json::json!("SECRET")),("evidence_sha256",serde_json::json!("SECRET")),("unknown",serde_json::json!("SECRET"))] {
                let mut invalid=value.clone(); invalid[key]=replacement;
                let hold=Hold::parse(&invalid);
                assert_eq!(hold.reason,Reason::FreshnessUnknown);
                assert!(!hold.to_string().contains("SECRET"));
                assert!(!serde_json::to_string(&hold).unwrap().contains("SECRET"));
            }
        }
    }
}
