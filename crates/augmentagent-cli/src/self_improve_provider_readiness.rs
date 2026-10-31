//! Read-only role admission, not proof of live execution or review approval.
use super::*;
use augmentagent_channel_core::{ProviderKind, providers::CapabilityClass};

#[derive(Debug, serde::Serialize)]
pub(crate) struct Role {
    status: &'static str,
    providers: Vec<String>,
    reasons: Vec<String>,
}
impl Role {
    fn available(providers: Vec<String>) -> Self {
        Self {status:"declared-available",providers,reasons:vec![]}
    }
}
#[derive(Debug, serde::Serialize)]
pub(crate) struct Report {
    pub builder: Role,
    pub independent_reviewer: Role,
    pub adjudicator: Role,
    pub live_verified: bool,
}
impl Report {
    pub(crate) fn preflight(&self) -> Result<()> {
        for (name,role) in [("builder",&self.builder),("independent-reviewer",&self.independent_reviewer),("adjudicator",&self.adjudicator)] {
            anyhow::ensure!(role.status=="declared-available","workflow-role:{name}:{}: {}",role.status,role.reasons.join("; "));
        }
        Ok(())
    }
}

pub(crate) fn inspect() -> Report {
    let chain=build_reasoner();
    let providers=chain.lane_provider_availability(CapabilityClass::FullAgentic);
    let available:Vec<_>=providers.iter().filter(|(_,until)|until.is_none()).map(|(kind,_)|*kind).collect();
    let mut builder_reasons=Vec::new();
    if providers.is_empty() {
        match augmentagent_channel_core::model_selection::current() {
            Err(_)=>builder_reasons.push("model-selection-invalid".into()),
            Ok(selected)=>{
                if augmentagent_channel_core::model_router::current().is_err() {
                    builder_reasons.push("model-router-invalid".into());
                }
                for kind in selected.map(|kind|vec![kind]).unwrap_or_else(augmentagent_channel_core::providers::chain_from_env) {
                    let reason=augmentagent_channel_core::fallback::workflow_native_ineligible_reason(kind)
                        .unwrap_or_else(||"selected-route-or-capability-unavailable".into());
                    builder_reasons.push(format!("{}: {reason}",kind.name()));
                }
            }
        }
    } else {
        builder_reasons=providers.iter().filter(|(_,until)|until.is_some())
            .map(|(kind,until)|format!("{}: cooldown until {until:?}",kind.name())).collect();
    }
    let builder=if !available.is_empty() {Role::available(available.iter().map(|kind|kind.name().into()).collect())}
        else {Role {status:if providers.is_empty(){"no-eligible-provider"}else{"provider-cooldown"},providers:vec![],
            reasons:builder_reasons}};
    // Before selection, consider every available builder the chain may use.
    // Actual review still loads durable authorship and requires an independent
    // fresh-session receipt. This report cannot supply or replace that receipt.
    let independent_reviewer=if available.is_empty() {Role{status:"builder-unavailable",providers:vec![],reasons:vec![]}}
        else {match select_session_isolated_reviewer(&reviewer_pool(),Ok(Some(available)),reviewer_status) {
            Ok(kind)=>Role::available(vec![kind.name().into()]),
            Err(reason)=>Role{status:"independent-reviewer-unavailable",providers:vec![],reasons:vec![reason.log_line()]},
        }};
    // Final authority uses native Codex even when another provider built or
    // reviewed the candidate. A usable chat route cannot substitute for it.
    let adjudicator=match reviewer_status(ProviderKind::Codex) {
        ReviewerStatus::Ready=>Role::available(vec!["codex".into()]),
        ReviewerStatus::Latched(until)=>Role{status:"provider-cooldown",providers:vec![],reasons:vec![format!("codex: cooldown until {until:?}")]},
        ReviewerStatus::NotConfigured(reason)=>Role{status:"provider-unavailable",providers:vec![],reasons:vec![reason]},
    };
    Report{builder,independent_reviewer,adjudicator,live_verified:false}
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[tokio::test]
    async fn unavailable_roles_or_resources_stop_before_git_selection_or_state_creation() {
        const CHILD:&str="AUTOSHIP_ROLE_GATE_FIXTURE";
        if std::env::var_os(CHILD).is_none() {
            use sha2::{Digest,Sha256};
            for kind in ["roles","resources"] {
                let root=tempfile::tempdir().unwrap();
                let fixture=root.path().join("fixture");std::fs::create_dir(&fixture).unwrap();
                std::fs::set_permissions(&fixture,std::fs::Permissions::from_mode(0o700)).unwrap();
                let artifact=fixture.join("artifact");std::fs::write(&artifact,b"fixture").unwrap();
                std::fs::set_permissions(&artifact,std::fs::Permissions::from_mode(0o600)).unwrap();
                std::fs::write(fixture.join("manifest.json"),serde_json::json!({"version":1,"files":[{"path":"artifact","sha256":format!("{:x}",Sha256::digest(b"fixture"))}]}).to_string()).unwrap();
                let runtime=root.path().join("runtime.json");
                std::fs::write(&runtime,serde_json::json!({"qemu":artifact,"kernel":artifact,"busybox":artifact,"firmware":artifact,
                    "data_dir":fixture,"library_dir":fixture,"module_dir":fixture,"modules":[artifact],"memory_mb":1024}).to_string()).unwrap();
                std::fs::set_permissions(&runtime,std::fs::Permissions::from_mode(0o600)).unwrap();
                let providers=if kind=="resources" {
                    let claude=root.path().join("claude.json");let codex=root.path().join("codex.json");
                    std::fs::write(&claude,r#"{"claudeAiOauth":{"accessToken":"fixture","refreshToken":"fixture","expiresAt":9999999999999}}"#).unwrap();
                    std::fs::write(&codex,r#"{"OPENAI_API_KEY":"fixture"}"#).unwrap();
                    for path in [&claude,&codex] {std::fs::set_permissions(path,std::fs::Permissions::from_mode(0o600)).unwrap();}
                    serde_json::json!({"claude":{"gateway":{},"auth_file":claude,"auth_relative":".claude/.credentials.json"},
                        "codex":{"gateway":{},"auth_file":codex,"auth_relative":".codex/auth.json"}})
                } else {serde_json::json!({})};
                let policy=root.path().join("policy.json");
                std::fs::write(&policy,serde_json::json!({"version":1,"runtime_config":runtime,"guest_runtime":fixture,
                    "state_root":root.path().join("state"),"memory_mb":1024,"max_state_bytes":8589934592u64,"max_sessions":16,"providers":providers}).to_string()).unwrap();
                std::fs::set_permissions(&policy,std::fs::Permissions::from_mode(0o600)).unwrap();
                let workspace=root.path().join("workspace");std::fs::create_dir(&workspace).unwrap();
                let status=std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact","self_improve::provider_readiness::tests::unavailable_roles_or_resources_stop_before_git_selection_or_state_creation","--nocapture"])
                    .env_clear().env("PATH","/usr/bin:/bin").env("HOME",&workspace)
                    .env("XDG_STATE_HOME",workspace.join("state")).env("AUGMENTAGENT_NATIVE_VM_POLICY",policy)
                    .env("AUGMENTAGENT_REASONER_CHAIN","claude").env("CLAUDE_CLI","/usr/bin/true").env("CODEX_CLI","/usr/bin/true")
                    .env("AUTOSHIP_ADMISSION_CASE",kind).env(CHILD,&workspace).current_dir(&workspace).status().unwrap();
                assert!(status.success());
            }
            return;
        }
        let root=PathBuf::from(std::env::var_os(CHILD).unwrap());
        let error=super::super::run_once(&root,false).await.err().expect("missing admission must stop selection");
        let expected=if std::env::var("AUTOSHIP_ADMISSION_CASE").unwrap()=="resources" {"workflow-resource:declaration-missing"} else {"workflow-role:builder:no-eligible-provider"};
        assert!(format!("{error:#}").contains(expected),"{error:#}");
        assert_eq!(std::fs::read_dir(root).unwrap().count(),0,"no git, lock, workflow, model or build state before admission");
    }
}
