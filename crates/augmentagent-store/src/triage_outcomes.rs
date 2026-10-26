//! Outcome joins for triage evaluation. Missing owner evidence is unresolved,
//! never an inferred approval. Reports contain counts and opaque action IDs.
use crate::{Store, StoreResult};
use rusqlite::params;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerOutcome {
    Approved,
    Revised,
    Skipped,
    AgedOut,
    Unresolved,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutcomeCounts {
    pub approved: u64,
    pub revised: u64,
    pub skipped: u64,
    pub aged_out: u64,
    pub unresolved: u64,
}
impl OutcomeCounts {
    fn record(&mut self, outcome: OwnerOutcome) {
        match outcome {
            OwnerOutcome::Approved => self.approved += 1,
            OwnerOutcome::Revised => self.revised += 1,
            OwnerOutcome::Skipped => self.skipped += 1,
            OwnerOutcome::AgedOut => self.aged_out += 1,
            OwnerOutcome::Unresolved => self.unresolved += 1,
        }
    }
    pub fn total(&self) -> u64 {
        self.approved + self.revised + self.skipped + self.aged_out + self.unresolved
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisagreementCase {
    pub action_id: String,
    pub owner_outcome: OwnerOutcome,
    pub evidence: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriageOutcomeReport {
    pub since_ms: i64,
    pub flagged: OutcomeCounts,
    pub pending: OutcomeCounts,
    /// Historical actions without a trustworthy immutable decision snapshot.
    #[serde(default)]
    pub unknown_cohort: OutcomeCounts,
    /// Fraction of explicit owner resolutions that needed action. Aged-out and
    /// unresolved records are reported separately, not counted as skip/success.
    pub flag_precision: Option<f64>,
    pub observed_owner_resolutions: u64,
    pub flagged_then_aged_out: u64,
    /// Bounded review set, never automatically executable prompt/policy edits.
    pub disagreement_review_set: Vec<DisagreementCase>,
}

fn outcome(status: &str, verb: Option<&str>, revised: bool) -> OwnerOutcome {
    if status == "timed_out" { return OwnerOutcome::AgedOut; }
    match verb {
        Some("skip" | "reject" | "cancel") => OwnerOutcome::Skipped,
        Some("approve" | "send" | "send_now" | "schedule") if revised => OwnerOutcome::Revised,
        Some("approve" | "send" | "send_now" | "schedule") => OwnerOutcome::Approved,
        Some("revise") => OwnerOutcome::Revised,
        _ => OwnerOutcome::Unresolved,
    }
}
impl Store {
    /// Join action cohorts to durable, completed owner decisions. Current
    /// action status alone cannot tell an automatic send from owner approval.
    pub fn triage_outcome_report(
        &self,
        since_ms: i64,
        review_set_limit: usize,
    ) -> StoreResult<TriageOutcomeReport> {
        self.with_conn(|conn| {
            let mut statement = conn.prepare("SELECT a.id,a.status,COALESCE(c.cohort,''),
                (SELECT h.verb FROM approval_history h WHERE h.action_id=a.id
                 AND h.finished_at IS NOT NULL AND h.outcome IN ('completed','revised')
                 AND trim(h.actor) NOT IN ('','unknown')
                 ORDER BY h.seq DESC LIMIT 1),
                EXISTS(SELECT 1 FROM approval_history h WHERE h.action_id=a.id AND h.verb='revise'
                 AND h.outcome='revised' AND h.finished_at IS NOT NULL AND trim(h.actor) NOT IN ('','unknown'))
                FROM actions a JOIN emails e ON e.messageId=a.messageId
                LEFT JOIN triage_action_cohorts c ON c.action_id=a.id
                WHERE a.createdAt>=?1 AND (c.cohort IN ('flagged','pending') OR a.status IN ('flagged','pending','timed_out')
                 OR EXISTS(SELECT 1 FROM approval_history h WHERE h.action_id=a.id))
                ORDER BY a.createdAt DESC,a.id")?;
            let mut report = TriageOutcomeReport { since_ms, flagged: Default::default(), pending: Default::default(), unknown_cohort:Default::default(), flag_precision:None,
                observed_owner_resolutions:0, flagged_then_aged_out:0, disagreement_review_set:vec![] };
            let rows = statement.query_map(params![since_ms], |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?,row.get::<_,Option<String>>(3)?,row.get::<_,bool>(4)?)))?;
            for row in rows {
                let (action_id,status,triage,verb,revised) = row?;
                let result = outcome(&status,verb.as_deref(),revised);
                if triage == "flagged" {
                    report.flagged.record(result);
                    if result == OwnerOutcome::Skipped && !revised && report.disagreement_review_set.len() < review_set_limit.min(100) {
                        report.disagreement_review_set.push(DisagreementCase { action_id, owner_outcome:result,
                            evidence:"agent_flagged_owner_skipped_without_recorded_revision".into() });
                    }
                } else if triage == "pending" { report.pending.record(result); }
                else { report.unknown_cohort.record(result); }
            }
            report.observed_owner_resolutions = report.flagged.approved + report.flagged.revised + report.flagged.skipped;
            report.flag_precision = (report.observed_owner_resolutions > 0).then(||
                (report.flagged.approved + report.flagged.revised) as f64 / report.observed_owner_resolutions as f64);
            report.flagged_then_aged_out = report.flagged.aged_out;
            Ok(report)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval_history::DecisionContext;
    fn add(store: &Store, id: &str, triage: &str, status: &str, at: i64) {
        store.with_conn(|c| {
            c.execute("INSERT INTO emails(messageId,fromEmail,subject,triageResult,firstSeenAt) VALUES(?1,'sender@example.invalid','synthetic',?2,?3)",params![id,triage,at])?;
            c.execute("INSERT INTO actions(id,messageId,fromEmail,subject,status,createdAt,updatedAt) VALUES(?1,?1,'sender@example.invalid','synthetic',?2,?3,?3)",params![id,status,at])?;
            c.execute("INSERT OR REPLACE INTO triage_action_cohorts VALUES(?1,?2,?3)",params![id,if triage=="flag" {"flagged"} else {"pending"},at])?;
            Ok(())
        }).unwrap();
    }
    fn decision(store: &Store, id: &str, verb: &str, outcome: &str) {
        let ctx = DecisionContext {
            surface: "test".into(),
            actor: "owner".into(),
            conversation: "fixture".into(),
            interaction_id: format!("{id}-{verb}"),
            revision: None,
        };
        let seq = store
            .begin_approval(id, &ctx, verb, "v1", "email", "synthetic")
            .unwrap()
            .unwrap();
        store.finish_approval(seq, outcome, "").unwrap();
    }
    #[test]
    fn joins_actual_owner_outcomes_without_treating_missing_or_failed_evidence_as_success() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("db")).unwrap();
        for (id, status) in [
            ("approved", "sent"),
            ("revised", "sent"),
            ("skipped", "skipped"),
            ("aged", "timed_out"),
            ("missing", "sent"),
            ("failed", "error"),
        ] {
            add(&store, id, "flag", status, 100);
        }
        decision(&store, "approved", "approve", "completed");
        decision(&store, "revised", "revise", "revised");
        decision(&store, "revised", "approve", "completed");
        decision(&store, "skipped", "skip", "completed");
        decision(&store, "failed", "approve", "failed");
        add(&store, "pending", "reply", "pending", 100);
        add(&store, "old", "flag", "timed_out", 1);
        let report = store.triage_outcome_report(50, 10).unwrap();
        assert_eq!(
            report.flagged,
            OutcomeCounts {
                approved: 1,
                revised: 1,
                skipped: 1,
                aged_out: 1,
                unresolved: 2
            }
        );
        assert_eq!(report.pending.unresolved, 1);
        assert_eq!(report.flag_precision, Some(2.0 / 3.0));
        assert_eq!(report.flagged_then_aged_out, 1);
        assert_eq!(report.disagreement_review_set.len(), 1);
        assert_eq!(report.disagreement_review_set[0].action_id, "skipped");
        assert_eq!(
            store.triage_outcome_report(101, 10).unwrap().flag_precision,
            None
        );
    }
    #[test]
    fn disagreement_sample_is_bounded_and_excludes_revised_then_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("db")).unwrap();
        for id in ["one", "two", "edited"] {
            add(&store, id, "flag", "skipped", 100);
        }
        decision(&store, "edited", "revise", "revised");
        for id in ["one", "two", "edited"] {
            decision(&store, id, "skip", "completed");
        }
        let report = store.triage_outcome_report(0, 1).unwrap();
        assert_eq!(report.flagged.skipped, 3);
        assert_eq!(report.disagreement_review_set.len(), 1);
        assert_ne!(report.disagreement_review_set[0].action_id, "edited");
    }
    #[test]
    fn expired_revision_is_aged_out_not_precision_success() {
        let dir=tempfile::tempdir().unwrap();
        let store=Store::open(dir.path().join("db")).unwrap();
        add(&store,"expired-revision","flag","timed_out",100);
        decision(&store,"expired-revision","revise","revised");
        let report=store.triage_outcome_report(0,10).unwrap();
        assert_eq!(report.flagged.aged_out,1);
        assert_eq!(report.flagged.revised,0);
        assert_eq!(report.flagged_then_aged_out,1);
        assert_eq!(report.observed_owner_resolutions,0);
        assert_eq!(report.flag_precision,None);
    }

    #[test]
    fn original_cohorts_survive_retriage_and_unknown_history_is_not_inferred() {
        let dir=tempfile::tempdir().unwrap();
        let path=dir.path().join("db");
        let store=Store::open(&path).unwrap();
        store.with_conn(|c| {
            c.execute("INSERT INTO emails(messageId,fromEmail,subject,firstSeenAt) VALUES('message','sender@example.invalid','synthetic',100)",[])?;
            Ok(())
        }).unwrap();
        let pending=store.log_action("message",None,"sender@example.invalid","synthetic",None,Some("reply"),crate::ActionStatus::Pending).unwrap();
        store.update_action_status(&pending,crate::ActionStatus::Flagged,None,Some("retry escalation")).unwrap();
        store.mark_email_processed("message",crate::TriageResult::Flag).unwrap();
        let flagged=store.log_flagged_action("message",None,"sender@example.invalid","synthetic",None,"needs attention").unwrap();
        store.update_action_status(&flagged,crate::ActionStatus::Skipped,None,None).unwrap();
        decision(&store,&flagged,"skip","completed");
        store.mark_email_processed("message",crate::TriageResult::Reply).unwrap();
        // Simulate a pre-migration action whose mutable final status is known,
        // but original cohort is not. Neither status nor email label repairs it.
        let legacy=store.log_action("message",None,"sender@example.invalid","synthetic",None,None,crate::ActionStatus::Pending).unwrap();
        store.with_conn(|c| { c.execute("DELETE FROM triage_action_cohorts WHERE action_id=?1",params![legacy])?;Ok(()) }).unwrap();
        decision(&store,&legacy,"approve","completed");
        drop(store);
        let store=Store::open(path).unwrap();
        let report=store.triage_outcome_report(0,10).unwrap();
        assert_eq!(report.pending.unresolved,1,"reply escalation cannot rewrite original cohort");
        assert_eq!(report.flagged.skipped,1,"later message re-triage cannot erase original flag");
        assert_eq!(report.unknown_cohort.approved,1);
        assert_eq!(report.observed_owner_resolutions,1);
        assert_eq!(report.flag_precision,Some(0.0));
        assert_eq!(report.disagreement_review_set[0].action_id,flagged);
    }

}
