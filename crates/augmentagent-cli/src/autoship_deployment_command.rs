//! #1714: updater-facing deployment receipts. Local state authority only; this
//! never merges, deploys or sends a notification. It projects truthful
//! deployment progress onto confirmed autonomous merges.
use anyhow::Result;
use serde_json::json;
use std::path::PathBuf;
use crate::autoship_workflow::{DeploymentOutcome,DeploymentReceipt,WorkflowStore};

#[derive(clap::Args)]
pub(crate) struct Args {
    #[command(subcommand)] pub operation: Operation,
}
#[derive(Clone,Copy,clap::ValueEnum)]
pub(crate) enum Outcome {Deployed,Failed}
#[derive(clap::Subcommand)]
pub(crate) enum Operation {
    /// Declare whether merges in REPO are deployed by the normal updater.
    Applicability {
        #[arg(long)] repo:String,
        #[arg(long,action=clap::ArgAction::Set)] deployable:bool,
        #[arg(long)] json:bool,
    },
    /// Record one updater receipt for the built REVISION. `deployed` requires
    /// --artifact-verified (running executables checked against that build).
    Receipt {
        #[arg(long)] repo:String,
        #[arg(long)] revision:String,
        #[arg(long,value_enum)] outcome:Outcome,
        #[arg(long)] artifact_verified:bool,
        /// Git checkout used to prove each merge commit is in REVISION.
        #[arg(long)] checkout:Option<PathBuf>,
        #[arg(long)] json:bool,
    },
}

fn apply(path:&std::path::Path,operation:&Operation)->Result<serde_json::Value> {
    let mut store=WorkflowStore::open(path)?;
    Ok(match operation {
        Operation::Applicability {repo,deployable,..}=>{
            store.set_deployment_applicability(repo,*deployable)?;
            json!({"schema_version":1,"accepted":true,"repository":repo.to_ascii_lowercase(),"deployable":deployable})
        }
        Operation::Receipt {repo,revision,outcome,artifact_verified,checkout,..}=>{
            let checkout=match checkout {Some(path)=>path.clone(),None=>std::env::current_dir()?};
            let receipt=DeploymentReceipt {repository:repo.clone(),revision:revision.to_ascii_lowercase(),
                outcome:match outcome {Outcome::Deployed=>DeploymentOutcome::Deployed,Outcome::Failed=>DeploymentOutcome::Failed},
                artifact_verified:*artifact_verified};
            let advanced=store.record_deployment_receipt(&checkout,&receipt,chrono::Utc::now().timestamp_millis())?;
            json!({"schema_version":1,"accepted":true,"receipt":receipt,"advanced_workflows":advanced})
        }
    })
}

pub(crate) fn run(args:&Args)->i32 {
    let json_output=match &args.operation {Operation::Applicability {json,..}|Operation::Receipt {json,..}=>*json};
    let (code,value)=match apply(&crate::autoship_workflow::default_path(),&args.operation) {
        Ok(value)=>(0,value),
        Err(error)=>(3,json!({"schema_version":1,"accepted":false,"reason":"deployment-receipt-refused","error":format!("{error:#}")})),
    };
    if json_output {println!("{value}");}else {println!("{}",serde_json::to_string_pretty(&value).unwrap());}
    code
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    #[test]
    fn receipt_command_parses_and_refuses_unverified_or_unconfigured_deployments() {
        assert!(crate::Cli::try_parse_from(["augmentagent","autopr","deployment","receipt","--repo","owner/repo",
            "--revision",&"a".repeat(40),"--outcome","deployed","--artifact-verified","--json"]).is_ok());
        assert!(crate::Cli::try_parse_from(["augmentagent","autopr","deployment","applicability","--repo","owner/repo","--deployable","false"]).is_ok());
        assert!(crate::Cli::try_parse_from(["augmentagent","autopr","deployment","receipt","--repo","owner/repo","--revision","x","--outcome","merged"]).is_err());
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("state.db");
        let receipt=Operation::Receipt {repo:"owner/repo".into(),revision:"a".repeat(40),outcome:Outcome::Deployed,
            artifact_verified:true,checkout:Some(dir.path().into()),json:true};
        assert!(apply(&path,&receipt).is_err(),"unconfigured applicability must refuse rather than guess");
        apply(&path,&Operation::Applicability {repo:"Owner/Repo".into(),deployable:true,json:true}).unwrap();
        let unverified=Operation::Receipt {repo:"owner/repo".into(),revision:"a".repeat(40),outcome:Outcome::Deployed,
            artifact_verified:false,checkout:Some(dir.path().into()),json:true};
        assert!(apply(&path,&unverified).unwrap_err().to_string().contains("verified running artifact"));
    }
}
