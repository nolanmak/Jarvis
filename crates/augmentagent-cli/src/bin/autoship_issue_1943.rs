use std::fs;

fn contains_all(source: &str, terms: &[&str]) -> bool {
    terms.iter().all(|term| source.contains(term))
}

fn main() {
    let contract = fs::read_to_string("crates/augmentagent-cli/src/self_improve_contract.rs").unwrap_or_default();
    let workflow = fs::read_to_string("crates/augmentagent-cli/src/autoship_workflow.rs").unwrap_or_default();
    let progress = fs::read_to_string("crates/augmentagent-cli/src/autoship_progress.rs").unwrap_or_default();
    let source = fs::read_to_string("crates/augmentagent-cli/src/self_improve.rs").unwrap_or_default();

    let observed = [
        contains_all(&contract, &["initial_baseline", "correction", "register_and_reproduce"]),
        contains_all(&contract, &["BaselineEvidence", "persist_baseline", "contract_hash"]),
        contains_all(&workflow, &["scope_stage_response", "model_calls", "bounded_attempt"]),
        contains_all(&contract, &["BaselineNotRed", "InfrastructureFailure", "UnexpectedFailure"]),
        contains_all(&progress, &["Held", "command_attempt_finished", "workflow-progressed"]),
        contains_all(&source, &["merge_allowance", "candidate_passes", "NotReproduced"]),
    ];
    println!("{}", serde_json::to_string(&observed).unwrap());
}
