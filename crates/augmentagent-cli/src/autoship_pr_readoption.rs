//! Atomic owner PR readoption audit and shared command admission. This does
//! not reset a checkpoint, clear holds, or create review/merge evidence.
use super::*;

impl WorkflowStore {
    pub(crate) fn readopt_pr_authorized(
        &mut self,
        request_id: &str,
        principal: &str,
        request: &submission::Request,
        provenance: &serde_json::Value,
        now: i64,
    ) -> Result<(submission::Admission, bool)> {
        let request = request.normalized()?;
        let submission::Target::Pr(number) = request.target else {
            bail!("PR required")
        };
        let reason = provenance["reason"]
            .as_str()
            .context("readoption reason missing")?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(crate::self_improve::readoption::TABLE)?;
        let prior: Option<(String,String,u64,String,String,String)> = tx.query_row(
            "SELECT repository,target,number,reason,principal,prior_hold FROM owner_readoption WHERE request_id=?1",
            [request_id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional()?;
        if let Some((repository, target, prior_number, prior_reason, actor, prior_provenance)) =
            &prior
        {
            let previous: serde_json::Value = serde_json::from_str(prior_provenance)?;
            ensure!(
                repository == &request.repository
                    && target == "pr"
                    && *prior_number == number
                    && prior_reason == reason
                    && actor == principal
                    && previous["origin"] == provenance["origin"],
                "request-id-conflict"
            );
        }
        // The public ID belongs to the readoption/control intent. Keep its
        // admission ID separate so Discord can commit the durable readopt ACK
        // without mistaking the message for a second command kind.
        let admission_id = format!("pr-readopt:{:x}", Sha256::digest(request_id.as_bytes()));
        let admission =
            submission::submit_in_transaction(&tx, &admission_id, principal, &request, now)?;
        ensure!(
            admission.accepted,
            "readoption-admission-refused: {}",
            admission.reason
        );
        tx.execute("INSERT INTO owner_readoption(request_id,repository,target,number,reason,principal,prior_hold,status,created_at_ms,updated_at_ms)
            VALUES(?1,?2,'pr',?3,?4,?5,?6,'applied',?7,?7) ON CONFLICT(request_id) DO NOTHING",
            params![request_id,request.repository,number,reason,principal,serde_json::to_string(provenance)?,now])?;
        tx.commit()?;
        Ok((admission, prior.is_some()))
    }
}
