//! Protected execution context and scheduling hints. Never witness authority.
use super::*;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecoveryContext {
    pub version: u32,
    pub workflow: String,
    pub repository: String,
    pub worktree: std::path::PathBuf,
    pub source_request: String,
    pub source_pr: Option<u64>,
    pub contract: String,
    pub environment: String,
    pub binding: Binding,
}

pub(crate) struct RecoveryCandidate {
    pub snapshot: Snapshot,
    pub context: RecoveryContext,
}

fn context(
    conn: &Connection,
    workflow: &str,
    binding: &Binding,
) -> Result<Option<RecoveryContext>> {
    let exists:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='qualification_execution_context')",[],|r|r.get(0))?;
    if !exists {
        return Ok(None);
    }
    let saved:Option<(String,String)>=conn.query_row("SELECT key,payload FROM qualification_execution_context WHERE workflow=?1 AND binding=?2 ORDER BY seq DESC LIMIT 1",params![workflow,serde_json::to_string(binding)?],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    let Some((key, body)) = saved else {
        return Ok(None);
    };
    ensure!(
        format!("{:x}", Sha256::digest(body.as_bytes())) == key,
        "qualification context changed"
    );
    let context: RecoveryContext = serde_json::from_str(&body)?;
    ensure!(
        context.version == 1 && context.workflow == workflow && context.binding == *binding,
        "qualification context belongs to another binding"
    );
    Ok(Some(context))
}

pub(super) fn record_context(
    tx: &Transaction<'_>,
    lease: &Lease,
    checkpoint: &Checkpoint,
    context: &RecoveryContext,
    now: i64,
) -> Result<()> {
    ensure!(
        context.version == 1
            && context.workflow == lease.workflow
            && context.binding == checkpoint.binding
            && context.worktree.is_absolute()
            && !context.source_request.is_empty()
            && !context.contract.is_empty()
            && !context.environment.is_empty(),
        "qualification execution context changed"
    );
    let repository = submission::normalized_repository(&context.repository)?;
    ensure!(
        lease.workflow.starts_with(&format!("{repository}#"))
            || lease.workflow.starts_with(&format!("{repository}/pr/")),
        "qualification context repository differs"
    );
    let body = serde_json::to_string(context)?;
    ensure!(
        body.len() <= 2 * 1024 * 1024,
        "qualification execution context exceeds bound"
    );
    let key = format!("{:x}", Sha256::digest(body.as_bytes()));
    tx.execute_batch("CREATE TABLE IF NOT EXISTS qualification_execution_context(seq INTEGER PRIMARY KEY AUTOINCREMENT,workflow TEXT NOT NULL,key TEXT NOT NULL,binding TEXT NOT NULL,payload TEXT NOT NULL,at_ms INTEGER NOT NULL,UNIQUE(workflow,key));")?;
    tx.execute("INSERT OR IGNORE INTO qualification_execution_context(workflow,key,binding,payload,at_ms) VALUES(?1,?2,?3,?4,?5)",params![lease.workflow,key,serde_json::to_string(&context.binding)?,body,now])?;
    Ok(())
}

impl WorkflowStore {
    pub(crate) fn recovery_context(
        &self,
        workflow: &str,
        binding: &Binding,
    ) -> Result<Option<RecoveryContext>> {
        context(&self.conn, workflow, binding)
    }

    pub(crate) fn record_recovery_context(
        &mut self,
        lease: &Lease,
        binding: &Binding,
        context: &RecoveryContext,
        now: i64,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx, lease, binding, now)?;
        let raw: String = tx.query_row(
            "SELECT checkpoint FROM workflow WHERE id=?1",
            [&lease.workflow],
            |r| r.get(0),
        )?;
        record_context(&tx, lease, &serde_json::from_str(&raw)?, context, now)?;
        tx.commit()?;
        Ok(())
    }

    /// Reserve before remote lookup, so an inaccessible or invalid source also
    /// rotates. Selection never claims execution or consumes an attempt.
    pub(crate) fn reserve_qualification_recovery(
        &mut self,
        repository: &str,
        now: i64,
        limit: usize,
        active: &std::collections::BTreeSet<String>,
    ) -> Result<Vec<RecoveryCandidate>> {
        ensure!(
            (1..=4).contains(&limit),
            "qualification recovery window exceeds bound"
        );
        let repository = submission::normalized_repository(repository)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let rows = {
            let mut query=tx.prepare("SELECT w.id,w.checkpoint,w.generation,COALESCE((SELECT MAX(e.at_ms) FROM workflow_event e
                WHERE e.workflow=w.id AND e.kind='qualification_recovery_selected'
                AND json_extract(e.payload,'$.binding.head')=json_extract(w.checkpoint,'$.binding.head')
                AND json_extract(e.payload,'$.binding.contract')=json_extract(w.checkpoint,'$.binding.contract')
                AND json_extract(e.payload,'$.binding.evidence_hash')=json_extract(w.checkpoint,'$.binding.evidence_hash')),0) AS selected
                FROM workflow w WHERE (instr(w.id,?1||'#')=1 OR instr(w.id,?1||'/pr/')=1)
                AND w.lease_until<=?2 AND json_extract(w.checkpoint,'$.next.eligible_at_ms')<=?2
                AND json_extract(w.checkpoint,'$.stage')='scope'
                AND json_extract(w.checkpoint,'$.next.reason') IN ('acceptance_verifier_infrastructure_unavailable','acceptance_verifier_evidence_unknown','acceptance_verifier_repair_exhausted','acceptance_verifier_amendment_declined')
                AND NOT EXISTS(SELECT 1 FROM workflow_owner_control c WHERE c.workflow=w.id AND c.paused=1)
                ORDER BY selected,w.id LIMIT 128")?;
            let rows = query
                .query_map(params![repository, now], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        let mut selected = Vec::new();
        for (id, raw, generation, last) in rows {
            if active.contains(&id) {
                continue;
            }
            if selected.len() == limit {
                break;
            }
            if last > 0 && now < last.saturating_add(300_000) {
                continue;
            }
            let checkpoint: Checkpoint = serde_json::from_str(&raw)?;
            event(
                &tx,
                &id,
                generation,
                "qualification_recovery_selected",
                &serde_json::json!({"binding":checkpoint.binding}).to_string(),
                now,
            )?;
            if !verifier_custody::original_hold_matches(&tx, &id, &checkpoint).unwrap_or(false)
                && !qualification_recovery::physical_hold(&tx, &id, generation, &checkpoint)
                    .unwrap_or(false)
                // Completed negative judgments need read-only source inspection,
                // too. Selection grants no qualification or model admission.
                && !((qualification_recovery::native_hold(&checkpoint) || legacy_amendment_input(&checkpoint).is_some())
                    && baseline_native_custody(&tx, &id)
                        .is_ok_and(|custody| !custody.is_empty()))
            {
                continue;
            }
            if qualification_recovery::pending_mutation(&tx, &id)?
                || super::owner_inbox::pending(&tx, &id)? > 0
            {
                continue;
            }
            if let Ok(Some(context)) = context(&tx, &id, &checkpoint.binding) {
                selected.push((id, context));
            }
        }
        tx.commit()?;
        selected
            .into_iter()
            .map(|(id, context)| {
                Ok(RecoveryCandidate {
                    snapshot: self.snapshot(&id)?.context("recovery workflow missing")?,
                    context,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn contract_invalid_metadata_hold_enters_only_free_inspection() {
        for attached in [false,true] {
            let dir=tempfile::tempdir_in("/tmp").unwrap();let mut store=WorkflowStore::open(&dir.path().join("state.db")).unwrap();
            let id="owner/repo#7";let binding=Binding {head:"head".into(),contract:"contract".into(),evidence_hash:"environment".into()};
            store.register(id,"candidate",binding.clone(),1).unwrap();let lease=store.claim(id,"original",2,1000).unwrap().unwrap();
            if attached {store.attach_pr_in_repo(&lease,"owner/repo",42,2).unwrap();}
            store.record_recovery_context(&lease,&binding,&RecoveryContext {version:1,workflow:id.into(),repository:"owner/repo".into(),worktree:dir.path().into(),
                source_request:"original request".into(),source_pr:attached.then_some(42),contract:"original request".into(),environment:"environment".into(),binding:binding.clone()},3).unwrap();
            let purpose=format!("{BASELINE_NATIVE_PURPOSE_PREFIX}{}","a".repeat(64));
            store.charge_admitted_model(&lease,"original-call",3,None,Some((&purpose,&binding,1)),Some(submission::Provider::Codex),true).unwrap();
            let (raw,proof)=baseline_native_response_keys(&purpose).unwrap();
            store.record_scope_stage_response_pair(&lease,&binding,&raw,"scripted completed witness",&proof,"scripted private reference",4).unwrap();
            let held=Checkpoint {stage:Stage::Scope,binding:binding.clone(),evidence:vec![],next:NextAction {
                reason:"acceptance_contract_invalid".into(),action:"await_acceptance_input_change".into(),eligible_at_ms:0}};
            store.checkpoint(&lease,&held,5).unwrap();store.release(&lease,6).unwrap();
            let before=store.snapshot(id).unwrap().unwrap();
            assert!(store.claim_productive_for_binding(id,"unchanged",7,1000,&binding).unwrap().is_none(),"metadata hold became productive before inspection");
            let selected=store.reserve_qualification_recovery("owner/repo",10,1,&Default::default()).unwrap();
            assert_eq!(selected.len(),1,"contract-invalid metadata evidence never reaches free inspection (attached={attached})");
            assert_eq!(selected[0].snapshot.workflow,id);
            assert_eq!(store.snapshot(id).unwrap().unwrap(),before,"inspection selection claimed or charged productive work");
            assert!(store.reserve_qualification_recovery("owner/repo",11,1,&Default::default()).unwrap().is_empty(),"unchanged metadata inspection bypassed its five-minute fairness reservation");
        }
    }

    #[test]
    fn semantic_hold_enters_free_selection_and_queued_reconsideration_wakes_normal_work() {
        let dir=tempfile::tempdir().unwrap();let mut store=WorkflowStore::open(&dir.path().join("state.db")).unwrap();
        let id="owner/repo#7";let binding=Binding {head:"head".into(),contract:"contract".into(),evidence_hash:"evidence".into()};
        store.register(id,"candidate",binding.clone(),1).unwrap();let lease=store.claim(id,"original",2,1000).unwrap().unwrap();
        store.attach_pr_in_repo(&lease,"owner/repo",42,2).unwrap();
        store.record_recovery_context(&lease,&binding,&RecoveryContext {version:1,workflow:id.into(),repository:"owner/repo".into(),worktree:dir.path().into(),source_request:"original request".into(),source_pr:Some(42),contract:"original request".into(),environment:"environment".into(),binding:binding.clone()},3).unwrap();
        let purpose=format!("{BASELINE_NATIVE_PURPOSE_PREFIX}{}","a".repeat(64));
        store.charge_admitted_model(&lease,"original-call",3,None,Some((&purpose,&binding,1)),Some(submission::Provider::Codex),true).unwrap();
        let (raw,proof)=baseline_native_response_keys(&purpose).unwrap();
        store.record_scope_stage_response_pair(&lease,&binding,&raw,"scripted completed unknown",&proof,"scripted private reference",4).unwrap();
        let held=Checkpoint {stage:Stage::Scope,binding:binding.clone(),evidence:vec![],next:NextAction {reason:"acceptance_verifier_evidence_unknown".into(),action:"await_original_evidence_recovery".into(),eligible_at_ms:0}};
        store.checkpoint_native_custody_hold(&lease,&held,5,false).unwrap();store.release(&lease,6).unwrap();
        let before=store.snapshot(id).unwrap().unwrap();
        let selected=store.reserve_qualification_recovery("owner/repo",10,1,&Default::default()).unwrap();
        assert_eq!(selected.len(),1,"semantic holds never reach authenticated free inspection");
        assert_eq!(selected[0].snapshot.workflow,id);
        assert_eq!(store.snapshot(id).unwrap().unwrap(),before,"selection claimed or charged execution");
        assert!(store.recover_semantic_qualification(&before,None,&"b".repeat(64),11,|_,_|Ok(Some("c".repeat(64)))).unwrap());
        assert!(store.has_due_qualification_recovery(Some("owner/repo"),11).unwrap(),"semantic handoff never wakes existing normal dispatch");
        assert_eq!(store.due_qualification_recovery(Some("owner/repo"),11).unwrap(),vec![id.to_string()]);
        let after=store.snapshot(id).unwrap().unwrap();
        assert_eq!((after.attempts,after.model_calls,after.generation),(before.attempts,before.model_calls,before.generation));
    }

    #[test]
    fn qualification_context_and_checkpoint_commit_together_without_public_request_text() {
        let root = tempfile::tempdir().unwrap();
        let mut store = WorkflowStore::open(&root.path().join("state.db")).unwrap();
        let id = "owner/repo#1";
        let binding = Binding {
            head: "head".into(),
            contract: "contract".into(),
            evidence_hash: "evidence".into(),
        };
        store.register(id, "candidate", binding.clone(), 1).unwrap();
        let lease = store.claim(id, "owner", 2, 1000).unwrap().unwrap();
        let mut context = RecoveryContext {
            version: 1,
            workflow: id.into(),
            repository: "owner/repo".into(),
            worktree: root.path().into(),
            source_request: "PRIVATE_OWNER_REQUEST_SENTINEL".into(),
            source_pr: None,
            contract: "original request".into(),
            environment: "environment".into(),
            binding,
        };
        store
            .record_recovery_context(&lease, &context.binding, &context, 3)
            .unwrap();
        let before = store.snapshot(id).unwrap().unwrap();
        let mut checkpoint = before.checkpoint.clone();
        checkpoint.binding.head = "new-head".into();
        context.binding = checkpoint.binding.clone();
        store.conn.execute_batch("CREATE TRIGGER refuse_context BEFORE INSERT ON qualification_execution_context BEGIN SELECT RAISE(ABORT,'context storage unavailable'); END;").unwrap();
        assert!(store
            .checkpoint_with_execution_context(&lease, &checkpoint, &context, 4)
            .is_err());
        assert_eq!(store.snapshot(id).unwrap().unwrap(), before);
        assert!(store
            .recovery_context(id, &checkpoint.binding)
            .unwrap()
            .is_none());
        store
            .conn
            .execute_batch("DROP TRIGGER refuse_context;")
            .unwrap();
        store
            .checkpoint_with_execution_context(&lease, &checkpoint, &context, 5)
            .unwrap();
        assert_eq!(
            store
                .recovery_context(id, &checkpoint.binding)
                .unwrap()
                .unwrap()
                .source_request,
            context.source_request
        );
        assert!(!serde_json::to_string(&store.events(id).unwrap())
            .unwrap()
            .contains("PRIVATE_OWNER_REQUEST_SENTINEL"));
        assert!(store
            .recovery_context("owner/other#1", &checkpoint.binding)
            .unwrap()
            .is_none());
    }
    #[test]
    fn qualification_selection_rotates_without_spending_or_reclaiming_a_lease() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state.db");
        let mut store = WorkflowStore::open(&path).unwrap();
        for issue in 1..=3 {
            let id = format!("owner/repo#{issue}");
            let binding = Binding {
                head: format!("head-{issue}"),
                contract: "request".into(),
                evidence_hash: "evidence".into(),
            };
            store
                .register(&id, "candidate", binding.clone(), 1)
                .unwrap();
            let lease = store.claim(&id, "original", 2, 1000).unwrap().unwrap();
            store
                .record_recovery_context(
                    &lease,
                    &binding,
                    &RecoveryContext {
                        version: 1,
                        workflow: id.clone(),
                        repository: "owner/repo".into(),
                        worktree: root.path().to_owned(),
                        source_request: "request".into(),
                        source_pr: None,
                        contract: "request".into(),
                        environment: "environment".into(),
                        binding: binding.clone(),
                    },
                    3,
                )
                .unwrap();
            let body=serde_json::json!({"version":1,"kind":"original_verifier_plan","workflow":id,"binding":binding}).to_string();
            let key = format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&("original-verifier-plan-v1", &body)).unwrap())
            );
            store
                .record_verifier_evidence(
                    &lease,
                    &binding,
                    &key,
                    "original_verifier_plan",
                    &body,
                    4,
                )
                .unwrap();
            store
                .checkpoint_verifier_recovery_hold(
                    &lease,
                    &Checkpoint {
                        stage: Stage::Scope,
                        binding,
                        evidence: vec![],
                        next: NextAction {
                            reason: "acceptance_verifier_infrastructure_unavailable".into(),
                            action: "await_verifier_infrastructure_recovery".into(),
                            eligible_at_ms: 0,
                        },
                    },
                    5,
                    false,
                    Some(&key),
                )
                .unwrap();
            store.release(&lease, 6).unwrap();
        }
        let first = store
            .reserve_qualification_recovery("owner/repo", 10, 2, &Default::default())
            .unwrap();
        assert_eq!(
            first
                .iter()
                .map(|c| c.snapshot.workflow.as_str())
                .collect::<Vec<_>>(),
            vec!["owner/repo#1", "owner/repo#2"]
        );
        drop(store);
        let mut store = WorkflowStore::open(&path).unwrap();
        let next = store
            .reserve_qualification_recovery("owner/repo", 11, 2, &Default::default())
            .unwrap();
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].snapshot.workflow, "owner/repo#3");
        assert!(store
            .reserve_qualification_recovery("owner/repo_extra", 12, 2, &Default::default())
            .unwrap()
            .is_empty());
        assert!(store
            .reserve_qualification_recovery("owner/repo", 12, 2, &Default::default())
            .unwrap()
            .is_empty());
        for issue in 1..=3 {
            let state = store
                .snapshot(&format!("owner/repo#{issue}"))
                .unwrap()
                .unwrap();
            assert_eq!(state.attempts, 1);
            assert_eq!(state.model_calls, 0);
            assert_eq!(state.generation, 1);
        }
        assert_eq!(
            store
                .reserve_qualification_recovery("owner/repo", 300_010, 2, &Default::default())
                .unwrap()
                .len(),
            2
        );
    }
}
