//! S-owned progress from completed, controller-captured verifier commands.
use super::*;

#[derive(Serialize, Deserialize)]
struct CompletedStep {
    version: u32,
    binding: Binding,
    admission: String,
    semantic_sha256: String,
}

fn semantic_key(binding: &Binding, semantic_sha256: &str) -> Result<String> {
    ensure!(
        semantic_sha256.len() == 64 && semantic_sha256.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid guarded artifact digest"
    );
    Ok(serde_json::to_string(&(binding, semantic_sha256))?)
}

pub(super) fn event_key(payload: &str) -> Result<String> {
    let step: CompletedStep = serde_json::from_str(payload)?;
    ensure!(step.version == 1, "unsupported guarded progress evidence");
    step.binding.validate()?;
    semantic_key(&step.binding, &step.semantic_sha256)
}

impl WorkflowStore {
    /// The caller derives the semantic digest from supervisor-captured bytes,
    /// never provider narration. Admission/quiescence evidence stays immutable.
    /// Reusing historical commands cannot establish new execution progress.
    pub(crate) fn record_guarded_verifier_progress(
        &mut self,
        lease: &Lease,
        binding: &Binding,
        admission: &str,
        semantic_sha256: &str,
        now: i64,
    ) -> Result<bool> {
        let key = semantic_key(binding, semantic_sha256)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx, lease, binding, now)?;
        let generation: i64 = tx.query_row("SELECT generation FROM workflow_event WHERE workflow=?1 AND kind='verifier_evidence_recorded' AND json_extract(payload,'$.kind')='original_verifier_command_admission' AND json_extract(payload,'$.key')=?2 ORDER BY seq DESC LIMIT 1", params![lease.workflow, admission], |r| r.get(0))?;
        if generation != lease.generation {
            return Ok(false);
        }
        let raw = verifier_custody::protected_record(
            &tx,
            &lease.workflow,
            admission,
            "original_verifier_command_admission",
        )?;
        let admitted: serde_json::Value = serde_json::from_str(&raw)?;
        ensure!(
            admitted["version"] == 1
                && admitted["kind"] == "original_verifier_command_admission"
                && admitted["workflow"] == lease.workflow,
            "guarded command owner changed"
        );
        let plan = admitted["plan"]
            .as_str()
            .context("guarded command plan missing")?;
        let attempt = admitted["attempt"]
            .as_str()
            .context("guarded command attempt missing")?;
        let verifier = admitted["verifier"]
            .as_str()
            .context("guarded command verifier missing")?;
        ensure!(
            format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&(
                    "original-verifier-command-v1",
                    plan,
                    attempt,
                    verifier
                ))?)
            ) == admission,
            "guarded command identity changed"
        );
        let quiet_key = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&(
                "original-verifier-command-quiescent-v1",
                admission
            ))?)
        );
        let quiet = verifier_custody::protected_record(
            &tx,
            &lease.workflow,
            &quiet_key,
            "original_verifier_command_quiescent",
        )?;
        let proof: serde_json::Value = serde_json::from_str(&quiet)?;
        ensure!(
            proof["version"] == 1
                && proof["kind"] == "original_verifier_command_quiescent"
                && proof["workflow"] == lease.workflow
                && proof["admission_sha256"] == format!("{:x}", Sha256::digest(raw.as_bytes()))
                && proof["evidence"]["identity"] == admitted["identity"],
            "guarded command quiescence changed"
        );
        // Read the kernel again: a stored completion statement is not authority.
        augmentagent_channel_core::run_resources::verify_quiescent(&admitted["identity"])?;
        let seen:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM workflow_activity_progress WHERE workflow=?1 AND kind='guarded_step_completed' AND semantic_key=?2)",params![lease.workflow,key],|r|r.get(0))?;
        if seen {
            return Ok(false);
        }
        event(
            &tx,
            &lease.workflow,
            lease.generation,
            "guarded_step_completed",
            &serde_json::to_string(&CompletedStep {
                version: 1,
                binding: binding.clone(),
                admission: admission.into(),
                semantic_sha256: semantic_sha256.into(),
            })?,
            now,
        )?;
        tx.commit()?;
        Ok(true)
    }
}
