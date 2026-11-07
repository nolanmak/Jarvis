//! C-owned accounting of a single immutable CI plan. Command receipts are
//! protected host publications, not VM/model JSON and not final freshness proof.
use super::*;
use augmentagent_channel_core::tool_audit::ProtectedControllerReceipt;
use std::collections::BTreeMap;

pub(super) struct Coverage {
    planned: BTreeSet<String>,
    receipts: BTreeMap<String, String>,
}
impl Coverage {
    pub(super) fn from_plan(plan: &Plan) -> Self {
        Self { planned: plan.checks.iter().filter(|check|check.status=="ready")
            .map(|check|check.id.clone()).collect(), receipts: BTreeMap::new() }
    }
    pub(super) fn record(&mut self, id: &str, receipt: &ProtectedControllerReceipt,
        binding: &Binding, authority: &str) -> Result<()> {
        let value=receipt.value();
        ensure!(self.planned.contains(id) && !self.receipts.contains_key(id),
            "{PREREQUISITE}: repeated or unplanned CI completion");
        ensure!(value["kind"]=="shared_ci_command_complete" && value["version"]==1
            && value["operation"]=="step" && value["check"]==id && value["status"]=="passed"
            && value["exit_code"]==0 && value["shipping_authority"]==false
            && value["binding"]==serde_json::to_value(binding)?
            && value["controller_authority_sha256"]==authority
            && value["cache_preparation"]["binding"]["head"]==binding.verification_head
            && value["cache_preparation"]["binding"]["tree"]==binding.index_tree
            && value["cache_preparation"]["binding"]["contract_sha256"]==binding.contract_sha256
            && value["cache_preparation"]["binding"]["authority_sha256"]==authority,
            "{PREREQUISITE}: CI completion belongs to another source, command or authority");
        self.receipts.insert(id.to_owned(),receipt.payload_sha256().to_owned());
        Ok(())
    }
    pub(super) fn complete(&self) -> Result<&BTreeMap<String,String>> {
        if self.receipts.keys().cloned().collect::<BTreeSet<_>>()!=self.planned {
            return Err(crate::autoship_freshness::Hold::parse(&serde_json::json!({
                "version":1,"reason":"metadata_missing","recovery":"rebuild_source_artifacts",
                "evidence_sha256":digest(b"planned local CI coverage is incomplete")})).into());
        }
        Ok(&self.receipts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn incomplete_duplicate_and_foreign_completions_cannot_close_the_plan() {
        let control=tempfile::tempdir_in("/tmp").unwrap();
        let audit=workflow::workflow_audit(&control.path().join("workflow.db"),"ci-coverage",None).unwrap();
        let binding=super::super::tests::binding_fixture();let authority="a".repeat(64);
        let plan:Plan=serde_json::from_value(serde_json::json!({"version":1,"head":binding.verification_head,
            "base":binding.base,"contract_sha256":binding.contract_sha256,"complete":true,
            "checks":[{"id":"first","status":"ready"},{"id":"second","status":"ready"},
                {"id":"mac","status":"remote_required"},{"id":"irrelevant","status":"not_applicable"}]})).unwrap();
        let body=|id:&str|serde_json::json!({"kind":"shared_ci_command_complete","version":1,"operation":"step",
            "check":id,"status":"passed","exit_code":0,"shipping_authority":false,"binding":binding,
            "controller_authority_sha256":authority,"cache_preparation":{"binding":{
                "head":binding.verification_head,"tree":binding.index_tree,"contract_sha256":binding.contract_sha256,
                "authority_sha256":authority}}});
        let mut coverage=Coverage::from_plan(&plan);
        assert!(coverage.complete().unwrap_err().downcast_ref::<crate::autoship_freshness::Hold>().is_some());
        let first=audit.record_controller_receipt(body("first")).unwrap();
        coverage.record("first",&first,&binding,&authority).unwrap();
        assert!(coverage.complete().is_err());
        assert!(coverage.record("first",&first,&binding,&authority).is_err());
        assert!(coverage.record("second",&first,&binding,&authority).is_err());
        assert!(coverage.record("mac",&first,&binding,&authority).is_err());
        for fault in ["status","source","authority","kind","operation"] {
            let mut value=body("second");
            match fault {
                "status"=>value["status"]="failed".into(),
                "source"=>value["binding"]["head"]="other-head".into(),
                "authority"=>value["controller_authority_sha256"]="other-authority".into(),
                "kind"=>value["kind"]="model-ci-summary".into(),
                "operation"=>value["operation"]="targeted".into(),
                _=>unreachable!(),
            }
            let receipt=audit.record_controller_receipt(value).unwrap();
            assert!(coverage.record("second",&receipt,&binding,&authority).is_err(),"{fault}");
            assert!(coverage.complete().is_err());
        }
        let second=audit.record_controller_receipt(body("second")).unwrap();
        coverage.record("second",&second,&binding,&authority).unwrap();
        assert_eq!(coverage.complete().unwrap().len(),2);
        assert_eq!(coverage.complete().unwrap()["first"],first.payload_sha256());
        assert_eq!(coverage.complete().unwrap()["second"],second.payload_sha256());
    }
}
