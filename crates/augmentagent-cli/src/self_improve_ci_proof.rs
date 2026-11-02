//! C owns the one freshness contract. Input evidence carries no CI verdict;
//! only complete required-local CI may become a bound passing capability.
use super::*;
use augmentagent_channel_core::tool_audit::ProtectedControllerReceipt;
use serde::de::DeserializeOwned;
use std::collections::BTreeMap;

pub(crate) const REQUIRED_VERSION: u32 = 1;
const KIND: &str = "shared_ci_freshness";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", deny_unknown_fields, rename_all = "snake_case")]
pub(crate) enum Invocation {
    Step {
        check: String,
    },
    Targeted {
        packages: Vec<String>,
    },
    Baseline {
        packages: Vec<String>,
        filters: Vec<String>,
    },
    Repository {
        command_sha256: String,
    },
}
impl Invocation {
    pub(crate) fn digest(&self) -> Result<String> {
        let valid = match self {
            Self::Step { check } => {
                !check.is_empty() && check.len() <= 4096 && !check.chars().any(char::is_control)
            }
            Self::Targeted { packages } => !packages.is_empty() && valid_packages(packages),
            Self::Baseline { packages, filters } => {
                valid_packages(packages)
                    && filters.len() <= 256
                    && filters.iter().all(|name| {
                        !name.is_empty()
                            && name.len() <= 512
                            && name
                                .bytes()
                                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b':'))
                    })
            }
            Self::Repository { command_sha256 } => sha(command_sha256),
        };
        if !valid {
            return Err(hold(
                crate::autoship_freshness::Reason::InputCensusUnknown,
                "invalid declared CI invocation",
            ));
        }
        Ok(digest(&serde_json::to_vec(self)?))
    }
    fn operation(&self) -> &'static str {
        match self {
            Self::Step { .. } => "step",
            Self::Targeted { .. } => "targeted",
            Self::Baseline { .. } => "baseline",
            Self::Repository { .. } => "repository",
        }
    }
}
fn valid_packages(packages: &[String]) -> bool {
    packages.len() <= 128
        && packages.iter().all(|name| {
            name.len() <= 128
                && name
                    .as_bytes()
                    .first()
                    .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        })
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SourceMode {
    CommittedGit,
    StagedSnapshot,
    RegisteredHarness,
}

/// Trusted caller expectations, derived from its owned checkout/registration,
/// current runtime census and declared invocation, never from a model response.
pub(crate) struct InputExpectation {
    pub source: Binding,
    pub mode: SourceMode,
    pub generation: String,
    pub command: String,
    pub invocation: Invocation,
    pub authority_sha256: String,
    pub physical_cache_sha256: String,
    pub overlay_registration_sha256: Option<String>,
}
pub(crate) struct CiExpectation {
    pub workflow: String,
    pub binding: crate::autoship_workflow::Binding,
    pub source: Binding,
    pub authority_sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Inputs {
    source: Binding,
    mode: SourceMode,
    generation: String,
    command: String,
    invocation_sha256: String,
    authority_sha256: String,
    physical_cache_sha256: String,
    git_census_sha256: String,
    observed_inputs_sha256: String,
    generated_provenance_sha256: String,
    artifact_provenance_sha256: String,
    outputs_policy_sha256: String,
    #[serde(deserialize_with = "required_nullable")]
    dependency_seed: Option<String>,
    quiescence_receipt_sha256: String,
    #[serde(deserialize_with = "required_nullable")]
    overlay_receipt_sha256: Option<String>,
    source_read_only: bool,
    complete: bool,
}
// Null is an explicit observation of absence. Omitting the field is an
// incomplete manifest and must not silently deserialize to the same evidence.
pub(super) fn required_nullable<'de, D, T>(deserializer: D) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: DeserializeOwned,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    serde_json::from_value(value).map_err(serde::de::Error::custom)
}
/// The producer may construct this only after measuring complete input and
/// artifact provenance. There is deliberately no JSON/public constructor.
pub(super) struct ObservedInputs {
    inputs: Inputs,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields, rename_all = "snake_case")]
enum Claim {
    Inputs {
        inputs: Inputs,
    },
    PassingCi {
        workflow: String,
        binding: crate::autoship_workflow::Binding,
        execution_receipt_sha256: String,
    },
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    actor: String,
    kind: String,
    version: u32,
    claim: Claim,
}

/// Opaque, non-serializable input capability. It is not accepted by M.
pub(crate) struct ValidatedInputs {
    reference: String,
    inputs: Inputs,
}
impl ValidatedInputs {
    pub(crate) fn reference(&self) -> &str {
        &self.reference
    }
    pub(crate) fn generation(&self) -> &str {
        &self.inputs.generation
    }
}
/// Opaque, non-serializable required-local-CI capability. Hosted required checks,
/// authorization and independent exact-head review remain separate gates.
pub(crate) struct PassingCiProof {
    reference: String,
    binding: crate::autoship_workflow::Binding,
    remote_required: Vec<String>,
}
impl PassingCiProof {
    pub(crate) fn reference(&self) -> &str {
        &self.reference
    }
    pub(crate) fn binding(&self) -> &crate::autoship_workflow::Binding {
        &self.binding
    }
    pub(crate) fn remote_required(&self) -> &[String] {
        &self.remote_required
    }
}
fn hold(reason: crate::autoship_freshness::Reason, detail: &str) -> anyhow::Error {
    crate::autoship_freshness::Hold::parse(&serde_json::json!({"version":1,"reason":reason,
        "recovery":reason.recovery(),"evidence_sha256":digest(detail.as_bytes())}))
    .into()
}
fn sha(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| matches!(b,b'0'..=b'9'|b'a'..=b'f'))
}
fn oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|b| matches!(b,b'0'..=b'9'|b'a'..=b'f'))
}
fn uuid(value: &str) -> bool {
    uuid::Uuid::parse_str(value)
        .is_ok_and(|id| id.get_version_num() == 4 && id.to_string() == value)
}
fn read<T: DeserializeOwned>(
    audit: &augmentagent_channel_core::AuditLogger,
    reference: &str,
) -> Result<T> {
    use crate::autoship_freshness::Reason;
    if !sha(reference) {
        return Err(hold(
            Reason::MetadataMissing,
            "protected receipt identity missing",
        ));
    }
    let receipt = audit.controller_receipt(reference).map_err(|_| {
        hold(
            Reason::MetadataMissing,
            "protected committed controller receipt unavailable",
        )
    })?;
    serde_json::from_value(receipt.value().clone())
        .map_err(|_| hold(Reason::MetadataTorn, "protected receipt schema mismatch"))
}
fn envelope(receipt: &ProtectedControllerReceipt) -> Result<Envelope> {
    use crate::autoship_freshness::Reason;
    let value = receipt.value();
    if value["kind"] != KIND || value["version"] != REQUIRED_VERSION {
        return Err(hold(
            Reason::LegacyFormat,
            "freshness contract version missing or unsupported",
        ));
    }
    let parsed: Envelope = serde_json::from_value(value.clone())
        .map_err(|_| hold(Reason::MetadataTorn, "freshness envelope schema mismatch"))?;
    if parsed.actor != "controller" {
        return Err(hold(
            Reason::MetadataTorn,
            "freshness publisher is not controller",
        ));
    }
    Ok(parsed)
}

pub(crate) fn load_inputs(
    audit: &augmentagent_channel_core::AuditLogger,
    reference: &str,
    expected: &InputExpectation,
) -> Result<ValidatedInputs> {
    use crate::autoship_freshness::Reason;
    let receipt = audit
        .controller_receipt(reference)
        .map_err(|_| hold(Reason::MetadataMissing, "input receipt not committed"))?;
    let Claim::Inputs { inputs } = envelope(&receipt)?.claim else {
        return Err(hold(
            Reason::InputCensusUnknown,
            "receipt is not input evidence",
        ));
    };
    validate_inputs(audit, &inputs, expected)?;
    Ok(ValidatedInputs {
        reference: reference.to_owned(),
        inputs,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Quiescent {
    actor: String,
    kind: String,
    operation: String,
    binding: Binding,
    controller_authority_sha256: String,
    command: String,
    generation: String,
    proof: serde_json::Value,
    shipping_authority: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Overlay {
    actor: String,
    kind: String,
    version: u32,
    registration_sha256: String,
    original_head: String,
    observed_head: String,
    observed_tree: String,
    original_git_census_sha256: String,
    manifest_sha256: String,
    original_tracked_unchanged: bool,
    shipping_authority: bool,
}
fn validate_source(source: &Binding) -> Result<()> {
    use crate::autoship_freshness::Reason;
    if ![
        &source.head,
        &source.verification_head,
        &source.base,
        &source.index_tree,
    ]
    .into_iter()
    .all(|s| oid(s))
        || !sha(&source.contract_sha256)
        || !sha(&source.runner_sha256)
        || source.base_ref.is_empty()
        || source.base_ref.len() > 1024
        || source.base_ref.contains('\0')
        || source.status.len() > 1024 * 1024
    {
        return Err(hold(
            Reason::InputCensusUnknown,
            "invalid original/observed source binding",
        ));
    }
    Ok(())
}
fn validate_inputs(
    audit: &augmentagent_channel_core::AuditLogger,
    inputs: &Inputs,
    expected: &InputExpectation,
) -> Result<()> {
    use crate::autoship_freshness::Reason;
    validate_source(&expected.source)?;
    validate_source(&inputs.source)?;
    if inputs.source != expected.source
        || inputs.mode != expected.mode
        || inputs.generation != expected.generation
        || inputs.command != expected.command
        || !uuid(&inputs.generation)
        || !uuid(&inputs.command)
        || inputs.invocation_sha256 != expected.invocation.digest()?
    {
        return Err(hold(
            Reason::GenerationMismatch,
            "input evidence belongs to another source generation or invocation",
        ));
    }
    if !sha(&expected.authority_sha256) || inputs.authority_sha256 != expected.authority_sha256 {
        return Err(hold(
            Reason::AuthorityChanged,
            "trusted controller/runtime/toolchain authority changed",
        ));
    }
    if !sha(&expected.physical_cache_sha256)
        || inputs.physical_cache_sha256 != expected.physical_cache_sha256
        || inputs.dependency_seed.as_deref().is_some_and(|s| !sha(s))
    {
        return Err(hold(
            Reason::ArtifactProvenanceUnknown,
            "physical cache or dependency artifact provenance unknown",
        ));
    }
    if !inputs.complete
        || !inputs.source_read_only
        || ![
            &inputs.git_census_sha256,
            &inputs.observed_inputs_sha256,
            &inputs.generated_provenance_sha256,
            &inputs.artifact_provenance_sha256,
            &inputs.outputs_policy_sha256,
        ]
        .into_iter()
        .all(|s| sha(s))
    {
        return Err(hold(
            Reason::InputCensusUnknown,
            "source/generated/compiler input provenance incomplete",
        ));
    }
    match inputs.mode {
        SourceMode::CommittedGit => {
            if inputs.source.head != inputs.source.verification_head
                || !inputs.source.status.is_empty()
                || inputs.overlay_receipt_sha256.is_some()
                || expected.overlay_registration_sha256.is_some()
            {
                return Err(hold(
                    Reason::InputCensusUnknown,
                    "committed source cannot hide an overlay or staged replacement",
                ));
            }
        }
        SourceMode::StagedSnapshot => {
            if inputs.source.head == inputs.source.verification_head
                || inputs.source.status.is_empty()
                || inputs.overlay_receipt_sha256.is_some()
                || expected.overlay_registration_sha256.is_some()
            {
                return Err(hold(
                    Reason::InputCensusUnknown,
                    "staged source must declare its distinct observed snapshot",
                ));
            }
        }
        SourceMode::RegisteredHarness => {
            let registration = expected
                .overlay_registration_sha256
                .as_deref()
                .filter(|s| sha(s))
                .ok_or_else(|| {
                    hold(
                        Reason::InputCensusUnknown,
                        "registered fixture authority missing",
                    )
                })?;
            let reference = inputs.overlay_receipt_sha256.as_deref().ok_or_else(|| {
                hold(
                    Reason::InputCensusUnknown,
                    "immutable registered fixture receipt missing",
                )
            })?;
            let overlay: Overlay = read(audit, reference)?;
            if overlay.actor != "controller"
                || overlay.kind != "shared_ci_registered_overlay"
                || overlay.version != REQUIRED_VERSION
                || overlay.registration_sha256 != registration
                || overlay.original_head != inputs.source.head
                || overlay.observed_head != inputs.source.verification_head
                || overlay.observed_tree != inputs.source.index_tree
                || !sha(&overlay.original_git_census_sha256)
                || !sha(&overlay.manifest_sha256)
                || !overlay.original_tracked_unchanged
                || overlay.shipping_authority
            {
                return Err(hold(
                    Reason::InputCensusUnknown,
                    "registered fixture changed or replaced original Git source",
                ));
            }
        }
    }
    let quiescent: Quiescent = read(audit, &inputs.quiescence_receipt_sha256).map_err(|_| {
        hold(
            Reason::CleanupUncertain,
            "original positive teardown receipt unavailable",
        )
    })?;
    if quiescent.actor != "controller"
        || quiescent.kind != "shared_ci_vm_quiescent"
        || quiescent.operation != expected.invocation.operation()
        || quiescent.binding != inputs.source
        || quiescent.controller_authority_sha256 != inputs.authority_sha256
        || quiescent.command != inputs.command
        || quiescent.generation != inputs.generation
        || !quiescent
            .proof
            .as_object()
            .is_some_and(|proof| !proof.is_empty())
        || quiescent.shipping_authority
    {
        return Err(hold(
            Reason::CleanupUncertain,
            "teardown evidence belongs to another execution",
        ));
    }
    Ok(())
}

pub(super) fn publish_inputs(
    audit: &augmentagent_channel_core::AuditLogger,
    observed: ObservedInputs,
    expected: &InputExpectation,
) -> Result<ValidatedInputs> {
    validate_inputs(audit, &observed.inputs, expected)?;
    let record = Envelope {
        actor: "controller".into(),
        kind: KIND.into(),
        version: REQUIRED_VERSION,
        claim: Claim::Inputs {
            inputs: observed.inputs,
        },
    };
    let receipt = audit
        .record_controller_receipt(serde_json::to_value(record)?)
        .map_err(|_| {
            hold(
                crate::autoship_freshness::Reason::MetadataMissing,
                "input publication did not commit",
            )
        })?;
    load_inputs(audit, receipt.payload_sha256(), expected)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Execution {
    actor: String,
    kind: String,
    run_id: String,
    binding: Binding,
    started_receipt_sha256: String,
    plan_receipt_sha256: String,
    command_receipts: BTreeMap<String, String>,
    input_provenance_status: String,
    reusable_passing_proof: bool,
    execution_boundary: String,
    vm_authority_sha256: String,
    remote_required: Vec<String>,
    shipping_authority: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Started {
    actor: String,
    kind: String,
    run_id: String,
    binding: Binding,
    shipping_authority: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeclaredCheck {
    id: String,
    status: String,
    reason: String,
    workflow: String,
    job: String,
    matrix: serde_json::Value,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Planned {
    actor: String,
    kind: String,
    run_id: String,
    binding: Binding,
    execution_boundary: String,
    vm_authority_sha256: String,
    checks: Vec<DeclaredCheck>,
    local_plan_complete: bool,
    quiescence_receipt_sha256: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Command {
    actor: String,
    kind: String,
    version: u32,
    operation: String,
    check: Option<String>,
    binding: Binding,
    controller_authority_sha256: String,
    cache_preparation: super::vm::CachePreparation,
    quiescence_receipt_sha256: String,
    status: String,
    exit_code: Option<i32>,
    stdout_sha256: String,
    stderr_sha256: String,
    shipping_authority: bool,
    #[serde(default)]
    invocation_sha256: Option<String>,
    #[serde(default)]
    input_attestation_sha256: Option<String>,
}
fn validate_execution(
    audit: &augmentagent_channel_core::AuditLogger,
    reference: &str,
    expected: &CiExpectation,
) -> Result<Vec<String>> {
    use crate::autoship_freshness::Reason;
    expected.binding.validate()?;
    validate_source(&expected.source)?;
    if expected.workflow.is_empty()
        || expected.workflow.len() > 1024
        || !sha(&expected.authority_sha256)
        || expected.source.head != expected.binding.head
        || expected.source.head != expected.source.verification_head
        || !expected.source.status.is_empty()
    {
        return Err(hold(
            Reason::GenerationMismatch,
            "passing CI requires the current clean committed candidate",
        ));
    }
    let execution: Execution = read(audit, reference)?;
    if execution.actor != "controller"
        || execution.kind != "shared_ci_local_complete"
        || !uuid(&execution.run_id)
        || execution.binding != expected.source
        || execution.vm_authority_sha256 != expected.authority_sha256
        || execution.execution_boundary != "existing-build-vm"
        || execution.shipping_authority
        || execution.reusable_passing_proof
    {
        return Err(hold(
            Reason::GenerationMismatch,
            "local completion belongs to another source or authority",
        ));
    }
    let latest = audit
        .latest_controller_receipt("shared_ci_started")
        .map_err(|_| {
            hold(
                Reason::MetadataMissing,
                "latest committed CI start unavailable",
            )
        })?
        .ok_or_else(|| hold(Reason::MetadataMissing, "required CI start missing"))?;
    if latest.payload_sha256() != execution.started_receipt_sha256 {
        return Err(hold(
            Reason::GenerationMismatch,
            "a newer CI run superseded this passing result",
        ));
    }
    if execution.input_provenance_status != "complete" {
        return Err(hold(
            Reason::InputCensusUnknown,
            "local check coverage alone is not complete input provenance",
        ));
    }
    let started: Started = read(audit, &execution.started_receipt_sha256)?;
    let plan: Planned = read(audit, &execution.plan_receipt_sha256)?;
    if started.actor != "controller"
        || started.kind != "shared_ci_started"
        || started.shipping_authority
        || started.run_id != execution.run_id
        || started.binding != expected.source
        || plan.actor != "controller"
        || plan.kind != "shared_ci_plan"
        || plan.run_id != execution.run_id
        || plan.binding != expected.source
        || plan.vm_authority_sha256 != expected.authority_sha256
        || plan.execution_boundary != "existing-build-vm"
        || !plan.local_plan_complete
        || plan.checks.is_empty()
    {
        return Err(hold(
            Reason::InputCensusUnknown,
            "completion lacks its original complete CI plan",
        ));
    }
    let planned_quiescence: Quiescent =
        read(audit, &plan.quiescence_receipt_sha256).map_err(|_| {
            hold(
                Reason::CleanupUncertain,
                "plan teardown publication missing",
            )
        })?;
    if planned_quiescence.actor != "controller"
        || planned_quiescence.kind != "shared_ci_vm_quiescent"
        || planned_quiescence.operation != "plan"
        || planned_quiescence.binding != expected.source
        || planned_quiescence.controller_authority_sha256 != expected.authority_sha256
        || !uuid(&planned_quiescence.command)
        || !uuid(&planned_quiescence.generation)
        || !planned_quiescence
            .proof
            .as_object()
            .is_some_and(|proof| !proof.is_empty())
        || planned_quiescence.shipping_authority
    {
        return Err(hold(
            Reason::CleanupUncertain,
            "plan used another execution's teardown",
        ));
    }
    let mut all = BTreeSet::new();
    let mut local = BTreeSet::new();
    let mut remote = BTreeSet::new();
    for check in &plan.checks {
        if check.id.is_empty() || check.id.len() > 4096 || !all.insert(check.id.clone()) {
            return Err(hold(
                Reason::InputCensusUnknown,
                "CI plan repeats or omits check identities",
            ));
        }
        match check.status.as_str() {
            "ready" => {
                local.insert(check.id.clone());
            }
            "remote_required" => {
                remote.insert(check.id.clone());
            }
            "not_applicable" => {}
            _ => {
                return Err(hold(
                    Reason::InputCensusUnknown,
                    "CI plan has unresolved applicability or prerequisites",
                ))
            }
        }
    }
    if local
        != execution
            .command_receipts
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>()
        || remote
            != execution
                .remote_required
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>()
        || remote.len() != execution.remote_required.len()
    {
        return Err(hold(
            Reason::InputCensusUnknown,
            "completed CI does not cover its entire original local plan",
        ));
    }
    let mut generation_groups = BTreeMap::new();
    let mut source_census = None;
    for check in plan.checks.iter().filter(|check| check.status == "ready") {
        let command: Command = read(audit, &execution.command_receipts[&check.id])?;
        let invocation = Invocation::Step {
            check: check.id.clone(),
        };
        if command.actor != "controller"
            || command.kind != "shared_ci_command_complete"
            || command.version != 1
            || command.operation != "step"
            || command.check.as_deref() != Some(check.id.as_str())
            || command.binding != expected.source
            || command.controller_authority_sha256 != expected.authority_sha256
            || command.status != "passed"
            || command.exit_code != Some(0)
            || command.shipping_authority
            || !sha(&command.stdout_sha256)
            || !sha(&command.stderr_sha256)
            || command.invocation_sha256.as_deref() != Some(invocation.digest()?.as_str())
        {
            return Err(hold(
                Reason::InputCensusUnknown,
                "required CI used incomplete, targeted or nonpassing command evidence",
            ));
        }
        let prepared = &command.cache_preparation;
        if prepared.version != 1
            || prepared.binding.head != expected.source.verification_head
            || prepared.binding.tree != expected.source.index_tree
            || prepared.binding.contract_sha256 != expected.source.contract_sha256
            || prepared.binding.authority_sha256 != expected.authority_sha256
        {
            return Err(hold(
                Reason::GenerationMismatch,
                "required command cache was prepared for another source",
            ));
        }
        let input_ref = command.input_attestation_sha256.as_deref().ok_or_else(|| {
            hold(
                Reason::InputCensusUnknown,
                "required command input attestation missing",
            )
        })?;
        let inputs = load_inputs(
            audit,
            input_ref,
            &InputExpectation {
                source: expected.source.clone(),
                mode: SourceMode::CommittedGit,
                generation: prepared.binding.generation.clone(),
                command: prepared.command.clone(),
                invocation,
                authority_sha256: expected.authority_sha256.clone(),
                physical_cache_sha256: prepared.binding.cache_identity_sha256.clone(),
                overlay_registration_sha256: None,
            },
        )?;
        if inputs.inputs.git_census_sha256 != prepared.binding.source_inputs_sha256
            || inputs.inputs.outputs_policy_sha256 != prepared.outputs_sha256
            || inputs.inputs.dependency_seed != prepared.dependency_seed
            || inputs.inputs.quiescence_receipt_sha256 != command.quiescence_receipt_sha256
            || inputs.inputs.generation == planned_quiescence.generation
        {
            return Err(hold(
                Reason::ArtifactProvenanceUnknown,
                "input proof and actual command cache/teardown disagree",
            ));
        }
        if source_census
            .as_ref()
            .is_some_and(|prior| prior != &inputs.inputs.git_census_sha256)
        {
            return Err(hold(
                Reason::InputCensusUnknown,
                "required checks compiled inconsistent original Git inputs",
            ));
        }
        source_census = Some(inputs.inputs.git_census_sha256.clone());
        let group = digest(&serde_json::to_vec(&(
            &check.workflow,
            &check.job,
            &check.matrix,
        ))?);
        if generation_groups
            .insert(inputs.inputs.generation.clone(), group.clone())
            .is_some_and(|prior| prior != group)
        {
            return Err(hold(
                Reason::GenerationMismatch,
                "different CI jobs shared a source generation",
            ));
        }
    }
    Ok(remote.into_iter().collect())
}

pub(crate) fn load_passing_ci(
    audit: &augmentagent_channel_core::AuditLogger,
    reference: &str,
    expected: &CiExpectation,
) -> Result<PassingCiProof> {
    use crate::autoship_freshness::Reason;
    let receipt = audit
        .controller_receipt(reference)
        .map_err(|_| hold(Reason::MetadataMissing, "passing CI receipt not committed"))?;
    let Claim::PassingCi {
        workflow,
        binding,
        execution_receipt_sha256,
    } = envelope(&receipt)?.claim
    else {
        return Err(hold(
            Reason::InputCensusUnknown,
            "input-only evidence cannot authorize required CI or merge",
        ));
    };
    if workflow != expected.workflow || binding != expected.binding {
        return Err(hold(
            Reason::GenerationMismatch,
            "passing CI belongs to another workflow/head/contract/evidence binding",
        ));
    }
    let remote_required = validate_execution(audit, &execution_receipt_sha256, expected)?;
    Ok(PassingCiProof {
        reference: reference.to_owned(),
        binding,
        remote_required,
    })
}

pub(super) fn publish_passing_ci(
    audit: &augmentagent_channel_core::AuditLogger,
    completed: &CompletedLocalCi,
    expected: &CiExpectation,
) -> Result<PassingCiProof> {
    validate_execution(audit, completed.receipt().payload_sha256(), expected)?;
    let record = Envelope {
        actor: "controller".into(),
        kind: KIND.into(),
        version: REQUIRED_VERSION,
        claim: Claim::PassingCi {
            workflow: expected.workflow.clone(),
            binding: expected.binding.clone(),
            execution_receipt_sha256: completed.receipt().payload_sha256().to_owned(),
        },
    };
    let receipt = audit
        .record_controller_receipt(serde_json::to_value(record)?)
        .map_err(|_| {
            hold(
                crate::autoship_freshness::Reason::MetadataMissing,
                "passing CI publication did not commit",
            )
        })?;
    load_passing_ci(audit, receipt.payload_sha256(), expected)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (
        tempfile::TempDir,
        std::sync::Arc<augmentagent_channel_core::AuditLogger>,
        Inputs,
        InputExpectation,
    ) {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let audit =
            workflow::workflow_audit(&dir.path().join("workflow.db"), "freshness-contract", None)
                .unwrap();
        let source = Binding {
            head: "1".repeat(40),
            verification_head: "1".repeat(40),
            base: "2".repeat(40),
            base_ref: "origin/main".into(),
            index_tree: "3".repeat(40),
            status: String::new(),
            contract_sha256: "4".repeat(64),
            runner_sha256: "5".repeat(64),
        };
        let generation = uuid::Uuid::new_v4().to_string();
        let command = uuid::Uuid::new_v4().to_string();
        let invocation = Invocation::Step {
            check: "fixture/test/0".into(),
        };
        let authority = "6".repeat(64);
        let quiescent=audit.record_controller_receipt(serde_json::json!({"kind":"shared_ci_vm_quiescent",
            "operation":"step","binding":source,"controller_authority_sha256":authority,
            "command":command,"generation":generation,"proof":{"version":1,"fixture":true},"shipping_authority":false})).unwrap();
        let expected = InputExpectation {
            source: source.clone(),
            mode: SourceMode::CommittedGit,
            generation: generation.clone(),
            command: command.clone(),
            invocation: invocation.clone(),
            authority_sha256: authority.clone(),
            physical_cache_sha256: "7".repeat(64),
            overlay_registration_sha256: None,
        };
        let inputs = Inputs {
            source,
            mode: SourceMode::CommittedGit,
            generation,
            command,
            invocation_sha256: invocation.digest().unwrap(),
            authority_sha256: authority,
            physical_cache_sha256: "7".repeat(64),
            git_census_sha256: "8".repeat(64),
            observed_inputs_sha256: "9".repeat(64),
            generated_provenance_sha256: "a".repeat(64),
            artifact_provenance_sha256: "b".repeat(64),
            outputs_policy_sha256: "c".repeat(64),
            dependency_seed: None,
            quiescence_receipt_sha256: quiescent.payload_sha256().into(),
            overlay_receipt_sha256: None,
            source_read_only: true,
            complete: true,
        };
        (dir, audit, inputs, expected)
    }
    fn save(audit: &augmentagent_channel_core::AuditLogger, inputs: Inputs) -> String {
        audit
            .record_controller_receipt(
                serde_json::to_value(Envelope {
                    actor: "controller".into(),
                    kind: KIND.into(),
                    version: REQUIRED_VERSION,
                    claim: Claim::Inputs { inputs },
                })
                .unwrap(),
            )
            .unwrap()
            .payload_sha256()
            .into()
    }
    #[test]
    fn committed_input_metadata_never_substitutes_for_bound_complete_inputs() {
        let (_dir, audit, original, expected) = fixture();
        let reference = save(&audit, original.clone());
        let validated = load_inputs(&audit, &reference, &expected).unwrap();
        assert_eq!(validated.reference(), reference);
        assert_eq!(validated.generation(), expected.generation);
        for fault in [
            "source",
            "generation",
            "command",
            "invocation",
            "authority",
            "cache",
            "unknown-inputs",
            "writable-source",
            "missing-quiescence",
            "overlay",
        ] {
            let mut inputs = original.clone();
            match fault {
                "source" => inputs.source.verification_head = "d".repeat(40),
                "generation" => inputs.generation = uuid::Uuid::new_v4().to_string(),
                "command" => inputs.command = uuid::Uuid::new_v4().to_string(),
                "invocation" => inputs.invocation_sha256 = "d".repeat(64),
                "authority" => inputs.authority_sha256 = "d".repeat(64),
                "cache" => inputs.physical_cache_sha256 = "d".repeat(64),
                "unknown-inputs" => inputs.complete = false,
                "writable-source" => inputs.source_read_only = false,
                "missing-quiescence" => inputs.quiescence_receipt_sha256 = "d".repeat(64),
                "overlay" => inputs.overlay_receipt_sha256 = Some("d".repeat(64)),
                _ => unreachable!(),
            }
            let reference = save(&audit, inputs);
            assert!(
                load_inputs(&audit, &reference, &expected).is_err(),
                "{fault} bypassed the shared input contract"
            );
        }
    }
    #[test]
    fn versioned_envelopes_and_source_modes_cannot_be_interchanged() {
        let (_dir, audit, original, mut expected) = fixture();
        for fault in [
            "old-version",
            "new-version",
            "extra-envelope",
            "extra-input",
            "missing-field",
            "missing-seed",
            "missing-overlay",
        ] {
            let mut value = serde_json::to_value(Envelope {
                actor: "controller".into(),
                kind: KIND.into(),
                version: REQUIRED_VERSION,
                claim: Claim::Inputs {
                    inputs: original.clone(),
                },
            })
            .unwrap();
            match fault {
                "old-version" => value["version"] = 0.into(),
                "new-version" => value["version"] = 2.into(),
                "extra-envelope" => value["unknown"] = true.into(),
                "extra-input" => value["claim"]["inputs"]["unknown"] = true.into(),
                "missing-seed" => {
                    value["claim"]["inputs"]
                        .as_object_mut()
                        .unwrap()
                        .remove("dependency_seed");
                }
                "missing-overlay" => {
                    value["claim"]["inputs"]
                        .as_object_mut()
                        .unwrap()
                        .remove("overlay_receipt_sha256");
                }
                _ => {
                    value["claim"]["inputs"]
                        .as_object_mut()
                        .unwrap()
                        .remove("generated_provenance_sha256");
                }
            }
            let receipt = audit.record_controller_receipt(value).unwrap();
            assert!(
                load_inputs(&audit, receipt.payload_sha256(), &expected).is_err(),
                "{fault}"
            );
        }
        let mut staged = original.clone();
        staged.mode = SourceMode::StagedSnapshot;
        staged.source.verification_head = "d".repeat(40);
        staged.source.status = "M  source.rs\n".into();
        expected.mode = staged.mode;
        expected.source = staged.source.clone();
        let r=audit.record_controller_receipt(serde_json::json!({"kind":"shared_ci_vm_quiescent","operation":"step",
            "binding":staged.source,"controller_authority_sha256":staged.authority_sha256,
            "command":staged.command,"generation":staged.generation,"proof":{"fixture":true},"shipping_authority":false})).unwrap();
        staged.quiescence_receipt_sha256 = r.payload_sha256().into();
        assert!(load_inputs(&audit, &save(&audit, staged.clone()), &expected).is_ok());
        expected.mode = SourceMode::CommittedGit;
        assert!(load_inputs(&audit, &save(&audit, staged.clone()), &expected).is_err());
        expected.mode = SourceMode::RegisteredHarness;
        expected.overlay_registration_sha256 = Some("e".repeat(64));
        staged.mode = SourceMode::RegisteredHarness;
        let overlay=audit.record_controller_receipt(serde_json::json!({"kind":"shared_ci_registered_overlay",
            "version":REQUIRED_VERSION,"registration_sha256":"e".repeat(64),"original_head":staged.source.head,
            "observed_head":staged.source.verification_head,"observed_tree":staged.source.index_tree,
            "original_git_census_sha256":"f".repeat(64),"manifest_sha256":"a".repeat(64),
            "original_tracked_unchanged":true,"shipping_authority":false})).unwrap();
        staged.overlay_receipt_sha256 = Some(overlay.payload_sha256().into());
        assert!(load_inputs(&audit, &save(&audit, staged.clone()), &expected).is_ok());
        expected.overlay_registration_sha256 = Some("f".repeat(64));
        assert!(load_inputs(&audit, &save(&audit, staged), &expected).is_err());
    }

    fn completed_fixture(
        audit: &augmentagent_channel_core::AuditLogger,
        inputs: &Inputs,
        expected: &InputExpectation,
        fault: &str,
    ) -> (CompletedLocalCi, CiExpectation) {
        let input_ref = save(audit, inputs.clone());
        let run = uuid::Uuid::new_v4().to_string();
        let source = &expected.source;
        let authority = &expected.authority_sha256;
        let started = audit
            .record_controller_receipt(serde_json::json!({"kind":"shared_ci_started","run_id":run,
            "binding":source,"shipping_authority":false}))
            .unwrap();
        let plan_q=audit.record_controller_receipt(serde_json::json!({"kind":"shared_ci_vm_quiescent","operation":"plan",
            "binding":source,"controller_authority_sha256":authority,"command":uuid::Uuid::new_v4().to_string(),
            "generation":uuid::Uuid::new_v4().to_string(),"proof":{"fixture":true},"shipping_authority":false})).unwrap();
        let check = "fixture/test/0";
        let mut plan = serde_json::json!({"kind":"shared_ci_plan","run_id":run,"binding":source,
            "execution_boundary":"existing-build-vm","vm_authority_sha256":authority,"local_plan_complete":true,
            "quiescence_receipt_sha256":plan_q.payload_sha256(),"checks":[
                {"id":check,"status":"ready","reason":"","workflow":"fixture.yml","job":"tests","matrix":{}},
                {"id":"fixture/mac","status":"remote_required","reason":"mac required","workflow":"fixture.yml","job":"mac","matrix":{}}]});
        if fault == "plan-run" {
            plan["run_id"] = uuid::Uuid::new_v4().to_string().into();
        }
        if fault == "unresolved-plan" {
            plan["checks"][0]["status"] = "prerequisite_hold".into();
        }
        let plan = audit.record_controller_receipt(plan).unwrap();
        let mut command = serde_json::json!({"kind":"shared_ci_command_complete","version":1,"operation":"step","check":check,
            "binding":source,"controller_authority_sha256":authority,"cache_preparation":{"version":1,
                "binding":{"generation":inputs.generation,"head":source.verification_head,"tree":source.index_tree,
                    "contract_sha256":source.contract_sha256,"authority_sha256":authority,
                    "cache_identity_sha256":inputs.physical_cache_sha256,"source_inputs_sha256":inputs.git_census_sha256},
                "command":inputs.command,"outputs_sha256":inputs.outputs_policy_sha256,"dependency_seed":null},
            "quiescence_receipt_sha256":inputs.quiescence_receipt_sha256,"status":"passed","exit_code":0,
            "stdout_sha256":"d".repeat(64),"stderr_sha256":"e".repeat(64),"shipping_authority":false,
            "invocation_sha256":inputs.invocation_sha256,"input_attestation_sha256":input_ref});
        match fault {
            "source-red" => {
                command["status"] = "failed".into();
                command["exit_code"] = 1.into();
            }
            "targeted" => command["operation"] = "targeted".into(),
            "invocation" => command["invocation_sha256"] = "f".repeat(64).into(),
            "missing-inputs" => {
                command
                    .as_object_mut()
                    .unwrap()
                    .remove("input_attestation_sha256");
            }
            "cache-provenance" => {
                command["cache_preparation"]["binding"]["source_inputs_sha256"] =
                    "f".repeat(64).into()
            }
            _ => {}
        }
        let command = audit.record_controller_receipt(command).unwrap();
        let mut commands =
            BTreeMap::from([(check.to_string(), command.payload_sha256().to_string())]);
        if fault == "partial-coverage" {
            commands.clear();
        }
        let execution=audit.record_controller_receipt(serde_json::json!({"kind":"shared_ci_local_complete","run_id":run,
            "binding":source,"started_receipt_sha256":started.payload_sha256(),"plan_receipt_sha256":plan.payload_sha256(),
            "command_receipts":commands,"input_provenance_status":if fault=="preparation-only" {"preparation_only"} else {"complete"},
            "reusable_passing_proof":false,"execution_boundary":"existing-build-vm","vm_authority_sha256":authority,
            "remote_required":if fault=="remote-omitted" {Vec::<String>::new()} else {vec!["fixture/mac".into()]},"shipping_authority":false})).unwrap();
        let ci = CiExpectation {
            workflow: "owner/repo#1".into(),
            binding: crate::autoship_workflow::Binding {
                head: source.head.clone(),
                contract: "owner-contract-v1".into(),
                evidence_hash: "f".repeat(64),
            },
            source: source.clone(),
            authority_sha256: authority.clone(),
        };
        (CompletedLocalCi { receipt: execution }, ci)
    }

    #[test]
    fn only_full_passing_coverage_can_publish_the_bound_ci_capability() {
        let (_dir, audit, inputs, expected) = fixture();
        let (completed, ci) = completed_fixture(&audit, &inputs, &expected, "none");
        let proof = publish_passing_ci(&audit, &completed, &ci).unwrap();
        assert_eq!(proof.binding(), &ci.binding);
        assert_eq!(proof.remote_required(), &["fixture/mac"]);
        assert_eq!(
            load_passing_ci(&audit, proof.reference(), &ci)
                .unwrap()
                .reference(),
            proof.reference()
        );
        let input_ref = save(&audit, inputs.clone());
        assert!(
            load_passing_ci(&audit, &input_ref, &ci).is_err(),
            "input-only evidence became merge authority"
        );
        assert!(load_inputs(&audit, proof.reference(), &expected).is_err());
        for fault in [
            "workflow",
            "head",
            "contract",
            "evidence",
            "authority",
            "tree",
            "ci-contract",
            "runner",
        ] {
            let mut other = CiExpectation {
                workflow: ci.workflow.clone(),
                binding: ci.binding.clone(),
                source: ci.source.clone(),
                authority_sha256: ci.authority_sha256.clone(),
            };
            match fault {
                "workflow" => other.workflow = "owner/repo#2".into(),
                "head" => other.binding.head = "d".repeat(40),
                "contract" => other.binding.contract = "owner-contract-v2".into(),
                "evidence" => other.binding.evidence_hash = "e".repeat(64),
                "authority" => other.authority_sha256 = "e".repeat(64),
                "tree" => other.source.index_tree = "d".repeat(40),
                "ci-contract" => other.source.contract_sha256 = "e".repeat(64),
                _ => other.source.runner_sha256 = "e".repeat(64),
            }
            assert!(
                load_passing_ci(&audit, proof.reference(), &other).is_err(),
                "{fault}"
            );
        }
        for fault in [
            "preparation-only",
            "partial-coverage",
            "source-red",
            "targeted",
            "invocation",
            "missing-inputs",
            "cache-provenance",
            "plan-run",
            "unresolved-plan",
            "remote-omitted",
        ] {
            let (incomplete, ci) = completed_fixture(&audit, &inputs, &expected, fault);
            assert!(
                publish_passing_ci(&audit, &incomplete, &ci).is_err(),
                "{fault} minted passing CI authority"
            );
        }
    }

    #[test]
    fn a_newer_pending_or_failed_ci_run_invalidates_the_previous_passing_proof() {
        let (_dir, audit, inputs, expected) = fixture();
        let (completed, ci) = completed_fixture(&audit, &inputs, &expected, "none");
        let proof = publish_passing_ci(&audit, &completed, &ci).unwrap();
        audit.record_controller_receipt(serde_json::json!({"kind":"shared_ci_started",
            "run_id":uuid::Uuid::new_v4().to_string(),"binding":ci.source,"shipping_authority":false})).unwrap();
        assert!(
            load_passing_ci(&audit, proof.reference(), &ci).is_err(),
            "a newer pending CI run at the same head reused the older passing proof"
        );
        let (failed, same) = completed_fixture(&audit, &inputs, &expected, "source-red");
        assert_eq!(same.source, ci.source);
        assert!(publish_passing_ci(&audit, &failed, &same).is_err());
        assert!(
            load_passing_ci(&audit, proof.reference(), &ci).is_err(),
            "a newer failed CI run at the same head fell back to an older passing proof"
        );
        assert!(
            audit.controller_receipt(proof.reference()).is_ok(),
            "historical audit evidence was deleted"
        );
    }

    #[test]
    fn invocation_identity_preserves_operation_selection_and_bounds() {
        let step = Invocation::Step {
            check: "fixture/test/0".into(),
        };
        let targeted = Invocation::Targeted {
            packages: vec!["crate_a".into()],
        };
        let baseline = Invocation::Baseline {
            packages: vec!["crate_a".into()],
            filters: vec!["tests::one".into()],
        };
        let repository = Invocation::Repository {
            command_sha256: "a".repeat(64),
        };
        let hashes =
            [step, targeted, baseline, repository].map(|invocation| invocation.digest().unwrap());
        assert_eq!(hashes.into_iter().collect::<BTreeSet<_>>().len(), 4);
        for invalid in [
            Invocation::Step { check: "\n".into() },
            Invocation::Targeted { packages: vec![] },
            Invocation::Targeted {
                packages: vec!["--all".into()],
            },
            Invocation::Baseline {
                packages: vec![],
                filters: vec!["--ignored".into()],
            },
            Invocation::Repository {
                command_sha256: "unknown".into(),
            },
        ] {
            assert!(invalid.digest().is_err());
        }
    }
}
