//! Read-only declarations. Credential usability is not live workflow readiness.
use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all="kebab-case")]
pub enum CredentialStatus {
    PolicyMissing, PolicyInvalid, RouteUndeclared, CredentialMissing,
    CredentialInvalid, CredentialExpired, CredentialExpiryUnknown,
    RefreshAuthorized, DeclaredUsable,
}

#[derive(Debug, Serialize)]
pub struct CredentialReport {
    pub provider: String,
    pub status: CredentialStatus,
    pub policy_sha256: Option<String>,
    pub expires_at_ms: Option<i64>,
    pub live_verified: bool,
}

pub fn credentials(provider:&str) -> CredentialReport {
    let path=std::env::var_os("AUGMENTAGENT_NATIVE_VM_POLICY");
    credentials_at(path.as_deref().map(Path::new),provider,chrono::Utc::now().timestamp_millis(),|key| {
        if key=="CODEX_API_KEY" {crate::secret_loader::load_provider_key(key)}
        else {std::env::var(key).ok()}
    })
}

impl CredentialReport {
    /// Admission may attempt an opaque token, but that is never a live-ready
    /// claim. Known expired credentials require an existing refresh grant.
    pub fn permits_attempt(&self) -> bool {
        matches!(self.status,CredentialStatus::DeclaredUsable|CredentialStatus::RefreshAuthorized|CredentialStatus::CredentialExpiryUnknown)
    }
    pub fn reason(&self) -> String {
        serde_json::to_value(self.status).expect("credential status serializes").as_str().unwrap().to_owned()
    }
}

pub fn credentials_at(path:Option<&Path>, provider:&str, now:i64,
    environment:impl Fn(&str)->Option<String>) -> CredentialReport {
    use sha2::{Digest,Sha256};
    use CredentialStatus::*;
    let mut report=CredentialReport {provider:provider.into(),status:PolicyMissing,
        policy_sha256:None,expires_at_ms:None,live_verified:false};
    let Some(path)=path else{return report};
    let bytes=match private(path) {Ok(bytes)=>bytes,Err(error)=>{
        report.status=if error.downcast_ref::<std::io::Error>().is_some_and(|error|error.kind()==std::io::ErrorKind::NotFound) {PolicyMissing} else {PolicyInvalid};return report
    }};
    report.policy_sha256=Some(format!("{:x}",Sha256::digest(&bytes)));
    let policy=match serde_json::from_slice::<Policy>(&bytes) {
        Ok(policy) if policy.version==1=>policy,
        _=>{report.status=PolicyInvalid;return report}
    };
    let Some(profile)=policy.providers.get(provider) else{report.status=RouteUndeclared;return report};
    let key=match provider {"codex"=>"CODEX_API_KEY","claude"=>"ANTHROPIC_API_KEY",_=>{report.status=RouteUndeclared;return report}};
    if profile.environment.iter().any(|name|name==key) && environment(key).is_some_and(|value|!value.trim().is_empty()) {
        report.status=DeclaredUsable;return report;
    }
    let Some(auth)=&profile.auth_file else{report.status=CredentialMissing;return report};
    let bytes=match private(auth) {
        Ok(bytes)=>bytes,
        Err(error)=>{report.status=if error.downcast_ref::<std::io::Error>().is_some_and(|error|error.kind()==std::io::ErrorKind::NotFound) {CredentialMissing} else {CredentialInvalid};return report}
    };
    let value:serde_json::Value=match serde_json::from_slice(&bytes) {Ok(value)=>value,Err(_)=>{report.status=CredentialInvalid;return report}};
    let nonempty=|pointer:&str|value.pointer(pointer).and_then(|v|v.as_str()).is_some_and(|value|!value.trim().is_empty());
    if provider=="codex" && nonempty("/OPENAI_API_KEY") {report.status=DeclaredUsable;return report;}
    let complete=if provider=="codex" {nonempty("/tokens/access_token") && nonempty("/tokens/refresh_token") && nonempty("/tokens/id_token")}
        else {nonempty("/claudeAiOauth/accessToken") && nonempty("/claudeAiOauth/refreshToken")};
    if !complete {report.status=CredentialMissing;return report;}
    if provider=="claude" && value.pointer("/claudeAiOauth/expiresAt").is_some_and(|v|v.as_i64().is_none()) {
        report.status=CredentialInvalid;return report;
    }
    report.expires_at_ms=if provider=="claude" {value.pointer("/claudeAiOauth/expiresAt").and_then(|v|v.as_i64())}
        else {jwt_expiry_ms(value.pointer("/tokens/access_token").and_then(|v|v.as_str()).unwrap_or(""))};
    let imported=policy.state_root.canonicalize().ok().map(|root|root.join("credentials").join(format!("{provider}.json")));
    if profile.refresh_auth && imported.as_ref()!=Some(auth) {report.status=CredentialInvalid;return report;}
    report.status=match report.expires_at_ms {
        None=>CredentialExpiryUnknown,
        Some(expiry) if expiry>now=>DeclaredUsable,
        Some(_)=> {
            if profile.refresh_auth && imported.as_ref()==Some(auth) {RefreshAuthorized} else {CredentialExpired}
        }
    };
    report
}

// Parsing an expiry is not signature verification or a successful login. It
// only lets us reject a token already known to have expired without sending it.
fn jwt_expiry_ms(token:&str) -> Option<i64> {
    use base64::Engine;
    let parts:Vec<_>=token.split('.').collect();
    if parts.len()!=3 {return None;}
    let bytes=base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(parts[1]).ok()?;
    let value:serde_json::Value=serde_json::from_slice(&bytes).ok()?;
    value["exp"].as_i64()?.checked_mul(1000)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn credential_readiness_distinguishes_expiry_and_preserves_private_state() {
        let root=tempfile::tempdir().unwrap();
        let path=root.path().join("policy.json");
        let auth=root.path().join("credentials/claude.json");
        std::fs::create_dir(auth.parent().unwrap()).unwrap();
        let mut policy=serde_json::json!({"version":1,"runtime_config":"/runtime.json","guest_runtime":"/guest",
            "state_root":root.path(),"memory_mb":1024,"max_state_bytes":8589934592u64,"max_sessions":16,
            "providers":{"claude":{"gateway":{},"auth_file":auth,"auth_relative":".claude/.credentials.json","refresh_auth":false}}});
        let write_policy=|value:&serde_json::Value| {
            std::fs::write(&path,serde_json::to_vec(value).unwrap()).unwrap();
            std::fs::set_permissions(&path,std::fs::Permissions::from_mode(0o600)).unwrap();
        };
        assert_eq!(credentials_at(None,"claude",1000,|_|None).status,CredentialStatus::PolicyMissing);
        write_policy(&policy);
        assert_eq!(credentials_at(Some(&path),"codex",1000,|_|None).status,CredentialStatus::RouteUndeclared);
        assert_eq!(credentials_at(Some(&path),"claude",1000,|_|None).status,CredentialStatus::CredentialMissing);
        for (expiry,expected) in [(Some(1001),CredentialStatus::DeclaredUsable),(Some(1000),CredentialStatus::CredentialExpired),
            (Some(999),CredentialStatus::CredentialExpired),(None,CredentialStatus::CredentialExpiryUnknown)] {
            let mut value=serde_json::json!({"claudeAiOauth":{"accessToken":"secret-access","refreshToken":"secret-refresh"}});
            if let Some(expiry)=expiry {value["claudeAiOauth"]["expiresAt"]=expiry.into();}
            std::fs::write(&auth,serde_json::to_vec(&value).unwrap()).unwrap();
            std::fs::set_permissions(&auth,std::fs::Permissions::from_mode(0o600)).unwrap();
            let before=std::fs::read(&auth).unwrap();
            let report=credentials_at(Some(&path),"claude",1000,|_|None);
            assert_eq!(report.status,expected);
            assert!(!report.live_verified);
            assert!(!serde_json::to_string(&report).unwrap().contains("secret-"));
            assert_eq!(std::fs::read(&auth).unwrap(),before);
        }
        std::fs::write(&auth,r#"{"claudeAiOauth":{"accessToken":"secret","refreshToken":"secret","expiresAt":999}}"#).unwrap();
        policy["providers"]["claude"]["refresh_auth"]=true.into();write_policy(&policy);
        assert_eq!(credentials_at(Some(&path),"claude",1000,|_|None).status,CredentialStatus::RefreshAuthorized);
        std::fs::set_permissions(&auth,std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(credentials_at(Some(&path),"claude",1000,|_|None).status,CredentialStatus::CredentialInvalid);
        std::fs::set_permissions(&path,std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(credentials_at(Some(&path),"claude",1000,|_|None).status,CredentialStatus::PolicyInvalid);
    }
}

#[cfg(test)]
mod jwt_tests {
    use super::*;
    #[test]
    fn expiry_is_metadata_not_live_authentication() {
        use base64::Engine;
        for (payload,expected) in [(serde_json::json!({"exp":1}),Some(1000)),
            (serde_json::json!({"exp":i64::MAX}),None),(serde_json::json!({}),None)] {
            let encoded=base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload.to_string());
            assert_eq!(jwt_expiry_ms(&format!("e30.{encoded}.signature")),expected);
        }
        assert_eq!(jwt_expiry_ms("opaque-subscription-token"),None);
        assert_eq!(jwt_expiry_ms("a.invalid!.b"),None);
    }
}
