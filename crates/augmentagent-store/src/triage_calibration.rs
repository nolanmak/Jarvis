//! Private, versioned triage calibration. Reports expose counts, never email bytes.
use crate::{redact, Store, StoreResult};
use rusqlite::{params, OptionalExtension};
tokio::task_local! { static INPUT:Option<i64>; }
pub async fn triage_calibration_scope<T>(
    input: Option<i64>,
    call: impl std::future::Future<Output = T>,
) -> T {
    INPUT.scope(input, call).await
}
pub(crate) fn attach(
    conn: &rusqlite::Connection,
    action: &str,
    message: &str,
) -> rusqlite::Result<()> {
    if let Some(input) = INPUT.try_with(|v| *v).ok().flatten() {
        conn.execute("UPDATE triage_action_cohorts SET input_id=?2 WHERE action_id=?1 AND input_id IS NULL AND EXISTS(SELECT 1 FROM triage_calibration_inputs i WHERE i.id=?2 AND i.message_id=?3 AND i.decision=CASE triage_action_cohorts.cohort WHEN 'flagged' THEN 'flag' ELSE 'reply' END)",params![action,input,message])?;
    }
    Ok(())
}
impl Store {
    /// Bind an action returned by this request's code-mode dispatcher after its
    /// async worker rejoins the owning request scope.
    pub fn attach_triage_calibration_action(&self, action: &str, message: &str) -> StoreResult<()> {
        self.with_conn(|c| attach(c, action, message))
    }

    /// Retain actual decision-time context; masked inputs are not replay labels.
    pub fn record_triage_calibration_input(
        &self,
        message: &str,
        prompt: &str,
        base: &str,
        model: &str,
        decision: &str,
        evidence: &str,
    ) -> StoreResult<i64> {
        let masked = redact::mask(prompt);
        let masked_evidence = redact::mask(evidence);
        let complete = serde_json::from_str::<serde_json::Value>(evidence)
            .ok()
            .is_some_and(|v| v["complete"] == true);
        self.with_conn(|c| {
            c.execute("INSERT INTO triage_calibration_inputs(message_id,prompt,base,model,decision,replayable,observed_at_ms,evidence) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                params![message,masked.as_ref(),base,model,decision,complete && masked.as_ref()==prompt && masked_evidence.as_ref()==evidence,chrono::Utc::now().timestamp_millis(),masked_evidence.as_ref()])?;
            Ok(c.last_insert_rowid())
        })
    }

    pub fn triage_calibration_policy(
        &self,
        base: &str,
        model: &str,
    ) -> StoreResult<Option<(String, String)>> {
        self.with_conn(|c|c.query_row("SELECT v.supplement,v.serving_model FROM triage_calibration_active a JOIN triage_calibration_versions v ON v.version=a.version WHERE a.singleton=1 AND v.base=?1 AND v.model=?2 AND a.disabled=0 AND v.serving_model!=''",params![base,model],|r|Ok((r.get(0)?,r.get(1)?))).optional())
    }
    pub fn triage_calibration_prompt(
        &self,
        base: &str,
        model: &str,
    ) -> StoreResult<Option<String>> {
        self.with_conn(|c| {
            c.query_row("SELECT v.supplement FROM triage_calibration_active a JOIN triage_calibration_versions v ON v.version=a.version WHERE a.singleton=1 AND v.base=?1 AND v.model=?2 AND a.disabled=0",params![base,model],|r|r.get(0)).optional()
        })
    }
}
