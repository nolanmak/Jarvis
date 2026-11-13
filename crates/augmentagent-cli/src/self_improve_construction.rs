//! Durable untrusted proposal production. A measurement intent is not a witness,
//! an environment approval, or permission to call the amendment authority.
use super::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn construction_output_cannot_smuggle_approval_or_drop_requirements() {
        let (_root,_control,old)=qualification_fixture();
        let mut proposal=old.proposal.clone();proposal.contract.revision+=1;
        proposal.files.values_mut().next().unwrap().push_str("\n# independently constructed correction\n");
        let response=serde_json::json!({"kind":"proposal","proposal":proposal,
            "additional_files":[],"propose_environment_transition":false});
        assert!(decode(&response.to_string(),&old,"different-environment",None).is_ok());
        let mut approval=response.clone();approval["approved"]=true.into();
        assert!(decode(&approval.to_string(),&old,"different-environment",None).is_err());
        let mut weakened=response.clone();weakened["proposal"]["contract"]["criteria"][0]["requirement"]="only check one route".into();
        assert!(decode(&weakened.to_string(),&old,"different-environment",None).is_err());
    }
}

fn decode(_raw:&str,_old:&PreparedAcceptance,_candidate_environment:&str,_adjudication:Option<&str>)->Result<()> {
    anyhow::bail!("constructor protocol not implemented")
}
