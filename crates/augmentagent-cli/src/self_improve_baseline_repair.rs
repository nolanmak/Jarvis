//! Bounded replay of rejected, unenrolled initial verifiers. Never proof/grants.
use super::*;

const LIMIT: u32 = 3;
const RECORD_LIMIT: usize = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(super) enum FailureKind {
    AlreadyPasses,
    NotReproduced,
    UnexpectedFailure,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Hold {
    Exhausted,
    EvidenceUnknown,
    ContractInvalid,
    InfrastructureUnavailable,
}
impl Hold {
    pub(super) fn reason(self) -> &'static str {
        match self {
            Self::Exhausted => "acceptance_verifier_repair_exhausted",
            Self::EvidenceUnknown => "acceptance_verifier_evidence_unknown",
            Self::ContractInvalid => "acceptance_contract_invalid",
            Self::InfrastructureUnavailable => "acceptance_verifier_infrastructure_unavailable",
        }
    }
    pub(super) fn action(self) -> &'static str {
        match self {
            Self::EvidenceUnknown => "await_original_evidence_recovery",
            Self::InfrastructureUnavailable => "await_verifier_infrastructure_recovery",
            _ => "await_acceptance_input_change",
        }
    }
}
impl std::fmt::Display for Hold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.reason(), self.action())
    }
}
impl std::error::Error for Hold {}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Failure {
    version: u32,
    epoch: String,
    slot: u32,
    proposal_sha256: String,
    #[serde(deserialize_with = "required_prior")]
    prior_sha256: Option<String>,
    kind: FailureKind,
    pub(super) detail: String,
}
fn required_prior<'de, D: serde::Deserializer<'de>>(
    decoder: D,
) -> std::result::Result<Option<String>, D::Error> {
    Option::<String>::deserialize(decoder)
}
fn sha(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn key(epoch: &str, slot: u32) -> String {
    amendment::digest(
        serde_json::to_string(&("initial-baseline-failure-v1", epoch, slot))
            .unwrap()
            .as_bytes(),
    )
}

#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct MeasurementIntent {
    version:u32,
    kind:String,
    epoch:String,
    slot:u32,
    proposal_sha256:String,
    #[serde(deserialize_with="required_prior")]
    prior_sha256:Option<String>,
}

fn intent_key(epoch:&str,slot:u32)->String {
    amendment::digest(&serde_json::to_vec(&("original-baseline-measurement-intent-v1",epoch,slot)).unwrap())
}

fn decode_intent(raw:&str,epoch:&str,slot:u32,prior:Option<&str>)->Result<MeasurementIntent> {
    (||->Result<MeasurementIntent> {
        ensure!(raw.len()<=4096,"measurement intent exceeds bound");
        let value:MeasurementIntent=serde_json::from_str(raw)?;
        ensure!(value.version==1 && value.kind=="original_measurement_intent"
            && value.epoch==epoch && sha(epoch) && value.slot==slot && slot<LIMIT
            && sha(&value.proposal_sha256) && value.prior_sha256.as_deref()==prior
            && value.prior_sha256.as_deref().is_none_or(sha),"measurement intent lineage mismatch");
        Ok(value)
    })().map_err(|e|e.context(Hold::EvidenceUnknown))
}
fn decode(raw: &str, epoch: &str, slot: u32, prior: Option<&str>) -> Result<Failure> {
    let checked = (|| -> Result<Failure> {
        ensure!(raw.len() <= RECORD_LIMIT, "failure record exceeds bound");
        let value: Failure = serde_json::from_str(raw)?;
        ensure!(
            value.version == 1
                && value.epoch == epoch
                && value.slot == slot
                && slot < LIMIT
                && sha(epoch)
                && sha(&value.proposal_sha256)
                && value.prior_sha256.as_deref() == prior
                && value.prior_sha256.as_deref().is_none_or(sha),
            "failure lineage mismatch"
        );
        Ok(value)
    })();
    checked.map_err(|_| Hold::EvidenceUnknown.into())
}

/// Callbacks are the existing immutable store, independent proposal generator,
/// and original baseline executor. B is returned only by the current executor;
/// successful observations are deliberately not cached in failure records.
pub(super) async fn run<P: Clone + Serialize, B, R, W, G, GF, O, OF>(
    epoch: &str,
    read: R,
    write: W,
    mut generate: G,
    mut observe: O,
) -> Result<(P, B)>
where
    R: Fn(&str) -> Result<Option<String>>,
    W: Fn(&str, &str) -> Result<()>,
    G: FnMut(u32, bool, Option<Failure>) -> GF,
    GF: std::future::Future<Output = Result<P>>,
    O: FnMut(P) -> OF,
    OF: std::future::Future<Output = Result<B>>,
{
    if !sha(epoch) {
        return Err(Hold::EvidenceUnknown.into());
    }
    let mut failures: Vec<(Failure, String)> = Vec::new();
    let mut gap = false;
    // Validate the complete existing prefix before any model/executor callback.
    for slot in 0..LIMIT {
        match read(&key(epoch, slot))? {
            Some(raw) => {
                if gap {
                    return Err(Hold::EvidenceUnknown.into());
                }
                let prior = failures.last().map(|(_, digest)| digest.as_str());
                let failure = decode(&raw, epoch, slot, prior)?;
                failures.push((failure, amendment::digest(raw.as_bytes())));
            }
            None => gap = true,
        }
    }
    // Retain only the original input identity, not another copy of a model
    // response. Recovery must still read the existing original proposal cache.
    // Validate every intent before invoking any generator or executor.
    let mut intents=Vec::new();
    for slot in 0..LIMIT {
        let intent=match read(&intent_key(epoch,slot))? {
            Some(raw)=> {
                if slot as usize>failures.len() {return Err(Hold::EvidenceUnknown.into());}
                let prior=slot.checked_sub(1).and_then(|s|failures.get(s as usize)).map(|(_,hash)|hash.as_str());
                let value=decode_intent(&raw,epoch,slot,prior)?;
                if let Some((failure,_))=failures.get(slot as usize) {
                    ensure!(value.proposal_sha256==failure.proposal_sha256,Hold::EvidenceUnknown);
                }
                Some(value)
            },
            None=>None,
        };
        intents.push(intent);
    }
    for slot in 0..LIMIT {
        let cached = (slot as usize) < failures.len();
        let previous = slot
            .checked_sub(1)
            .map(|prior| failures[prior as usize].0.clone());
        let intent=intents[slot as usize].as_ref();
        let proposal = generate(slot, cached || intent.is_some(), previous).await?;
        let proposal_sha256 = amendment::digest(&serde_json::to_vec(&proposal)?);
        if let Some(intent)=intent {
            ensure!(intent.proposal_sha256==proposal_sha256,Hold::EvidenceUnknown);
        }
        if cached {
            if proposal_sha256 != failures[slot as usize].0.proposal_sha256 {
                return Err(Hold::EvidenceUnknown.into());
            }
            continue;
        }
        if intent.is_none() {
            let record=MeasurementIntent {version:1,kind:"original_measurement_intent".into(),epoch:epoch.into(),slot,
                proposal_sha256:proposal_sha256.clone(),prior_sha256:failures.last().map(|(_,hash)|hash.clone())};
            write(&intent_key(epoch,slot),&serde_json::to_string(&record)?)?;
        }
        match observe(proposal.clone()).await {
            Ok(baseline) => return Ok((proposal, baseline)),
            Err(error) => {
                let Some(failed) = error.downcast_ref::<BaselineNotRed>() else {
                    return Err(error);
                };
                let Some(kind) = failed.kind else {
                    return Err(error.context(Hold::InfrastructureUnavailable));
                }; // retain infrastructure identity without consuming a slot
                let failure = Failure {
                    version: 1,
                    epoch: epoch.into(),
                    slot,
                    proposal_sha256,
                    prior_sha256: failures.last().map(|(_, digest)| digest.clone()),
                    kind,
                    detail: failed.detail.clone(),
                };
                let raw = serde_json::to_string(&failure)?;
                ensure!(
                    raw.len() <= RECORD_LIMIT,
                    "baseline failure record exceeds bound"
                );
                write(&key(epoch, slot), &raw)?;
                failures.push((failure, amendment::digest(raw.as_bytes())));
            }
        }
    }
    Err(Hold::Exhausted.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    };
    fn rejected() -> anyhow::Error {
        BaselineNotRed {
            detail: "unexpected fixture setup exit".into(),
            kind: Some(FailureKind::UnexpectedFailure),
        }
        .into()
    }

    fn record_counts(rows:&BTreeMap<String,String>)->(usize,usize) {
        let failures=rows.values().filter(|raw|serde_json::from_str::<Failure>(raw).is_ok()).count();
        let intents=rows.values().filter(|raw|serde_json::from_str::<MeasurementIntent>(raw).is_ok()).count();
        assert_eq!(failures+intents,rows.len(),"unexpected original observation record");
        (failures,intents)
    }

    #[tokio::test]
    async fn original_measurement_intent_is_durable_before_executor() {
        let records=Mutex::new(BTreeMap::<String,String>::new());
        let result=run(&"a".repeat(64),
            |key|Ok(records.lock().unwrap().get(key).cloned()),
            |key,raw|{records.lock().unwrap().insert(key.into(),raw.into());Ok(())},
            |_,cached,_|async move {assert!(!cached);Ok(41u32)},
            |proposal| {
                assert_eq!(proposal,41);
                assert_eq!(records.lock().unwrap().len(),1,"original measurement began without its retained input");
                std::future::ready(Ok::<_,anyhow::Error>("actual scripted observation"))
            }).await.unwrap();
        assert_eq!(result,(41,"actual scripted observation"));
    }

    #[tokio::test]
    async fn infrastructure_resume_only_reads_the_original_proposal() {
        let records=Mutex::new(BTreeMap::<String,String>::new());
        let fresh_proposals=AtomicUsize::new(0);
        for resume in [false,true] {
            let result=run(&"a".repeat(64),
                |key|Ok(records.lock().unwrap().get(key).cloned()),
                |key,raw|{records.lock().unwrap().insert(key.into(),raw.into());Ok(())},
                |slot,cached,_| {
                    assert_eq!(slot,0,"infrastructure failure consumed a semantic correction slot");
                    if !cached {fresh_proposals.fetch_add(1,Ordering::SeqCst);}
                    std::future::ready(Ok::<_,anyhow::Error>(41u32))
                },
                |proposal|std::future::ready(if resume {assert_eq!(proposal,41);Ok("recovered original measurement")}else {
                    Err(BaselineNotRed {detail:"original executor infrastructure failure".into(),kind:None}.into())
                })).await;
            if resume {assert_eq!(result.unwrap(),(41,"recovered original measurement"));}
            else {assert_eq!(result.unwrap_err().downcast_ref::<Hold>(),Some(&Hold::InfrastructureUnavailable));}
        }
        assert_eq!(fresh_proposals.load(Ordering::SeqCst),1,"infrastructure recovery requested a replacement proposal instead of the original cached result");
    }

    #[tokio::test]
    async fn invalid_original_measurement_intents_stop_before_callbacks() {
        let epoch="a".repeat(64);
        for fault in ["version","kind","epoch","slot","hash","prior","missing-prior","extra","gap"] {
            let mut value=serde_json::json!({"version":1,"kind":"original_measurement_intent","epoch":epoch,"slot":0,
                "proposal_sha256":amendment::digest(&serde_json::to_vec(&41u32).unwrap()),"prior_sha256":null});
            match fault {
                "version"=>value["version"]=2.into(),"kind"=>value["kind"]="approval".into(),
                "epoch"=>value["epoch"]="b".repeat(64).into(),"slot"|"gap"=>value["slot"]=1.into(),
                "hash"=>value["proposal_sha256"]="not-a-digest".into(),"prior"=>value["prior_sha256"]="b".repeat(64).into(),
                "missing-prior"=>{value.as_object_mut().unwrap().remove("prior_sha256");},
                "extra"=>value["approved"]=true.into(),_=>unreachable!(),
            }
            let selected=intent_key(&epoch,if fault=="gap" {1}else{0});let raw=value.to_string();
            let result=run(&epoch,|key|Ok((key==selected).then(||raw.clone())),
                |_,_|panic!("invalid original intent was overwritten"),
                |_,_,_|->std::future::Ready<Result<u32>> {panic!("invalid original intent requested a proposal")},
                |_|->std::future::Ready<Result<()>> {panic!("invalid original intent executed a measurement")}).await;
            assert_eq!(result.unwrap_err().downcast_ref::<Hold>(),Some(&Hold::EvidenceUnknown),"{fault}");
        }
    }

    #[tokio::test]
    async fn original_measurement_resume_refuses_missing_or_changed_proposal() {
        let epoch="a".repeat(64);let selected=intent_key(&epoch,0);
        let raw=serde_json::json!({"version":1,"kind":"original_measurement_intent","epoch":epoch,"slot":0,
            "proposal_sha256":amendment::digest(&serde_json::to_vec(&41u32).unwrap()),"prior_sha256":null}).to_string();
        for missing in [true,false] {
            let result=run(&epoch,|key|Ok((key==selected).then(||raw.clone())),
                |_,_|panic!("original intent was replaced"),
                |_,cached,_|std::future::ready({assert!(cached,"missing original result allowed a fresh request");if missing {Err(Hold::EvidenceUnknown.into())}else{Ok(42u32)}}),
                |_|->std::future::Ready<Result<()>> {panic!("unavailable original proposal allowed execution")}).await;
            assert_eq!(result.unwrap_err().downcast_ref::<Hold>(),Some(&Hold::EvidenceUnknown));
        }
    }

    #[tokio::test]
    async fn failed_measurement_retention_failure_stops_before_correction() {
        let models=AtomicUsize::new(0);let observations=AtomicUsize::new(0);
        let result=run(&"a".repeat(64),|_|Ok(None),
            |_,raw|if serde_json::from_str::<MeasurementIntent>(raw).is_ok() {Ok(())}else{anyhow::bail!("failed measurement retention unavailable")},
            |slot,_,_| {models.fetch_add(1,Ordering::SeqCst);std::future::ready(Ok(slot))},
            |_| {observations.fetch_add(1,Ordering::SeqCst);std::future::ready(Err::<(),_>(rejected()))}).await;
        assert!(result.unwrap_err().to_string().contains("failed measurement retention unavailable"));
        assert_eq!(models.load(Ordering::SeqCst),1);assert_eq!(observations.load(Ordering::SeqCst),1);
    }

    #[tokio::test]
    async fn invalid_original_and_first_correction_can_reach_second_independent_correction() {
        let records = Mutex::new(BTreeMap::<String, String>::new());
        let proposals = Mutex::new(BTreeMap::<u32, u32>::new());
        let models = AtomicUsize::new(0);
        let observations = AtomicUsize::new(0);
        for _ in 0..2 {
            let result = run(
                &"a".repeat(64),
                |key| Ok(records.lock().unwrap().get(key).cloned()),
                |key, raw| {
                    records.lock().unwrap().insert(key.into(), raw.into());
                    Ok(())
                },
                |slot, cached, previous| {
                    if slot > 0 {
                        assert!(previous.unwrap().detail.contains("fixture setup"));
                    }
                    let mut proposals = proposals.lock().unwrap();
                    let found = proposals.get(&slot).copied();
                    let result = match found {
                        Some(value) => Ok(value),
                        None if cached => Err(Hold::EvidenceUnknown.into()),
                        None => {
                            if slot > 0 {
                                models.fetch_add(1, Ordering::SeqCst);
                            }
                            proposals.insert(slot, slot);
                            Ok(slot)
                        }
                    };
                    async move { result }
                },
                |proposal| {
                    observations.fetch_add(1, Ordering::SeqCst);
                    async move {
                        if proposal < 2 {
                            Err(rejected())
                        } else {
                            Ok("scripted observation; not production BaselineEvidence")
                        }
                    }
                },
            )
            .await
            .unwrap();
            assert_eq!(result.0, 2);
        }
        assert_eq!(
            models.load(Ordering::SeqCst),
            2,
            "restart must reuse proposals"
        );
        assert_eq!(
            observations.load(Ordering::SeqCst),
            4,
            "successful evidence must come from the executor callback"
        );
        assert_eq!(record_counts(&records.lock().unwrap()),(2,3));
    }

    #[tokio::test]
    async fn exhausted_candidates_replay_without_models_or_observations() {
        let records = Mutex::new(BTreeMap::<String, String>::new());
        let calls = AtomicUsize::new(0);
        for pass in 0..2 {
            let result = run(
                &"b".repeat(64),
                |key| Ok(records.lock().unwrap().get(key).cloned()),
                |key, raw| {
                    records.lock().unwrap().insert(key.into(), raw.into());
                    Ok(())
                },
                |slot, cached, _| {
                    assert_eq!(cached, pass == 1);
                    async move { Ok(slot) }
                },
                |_| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    async { Err::<(), _>(rejected()) }
                },
            )
            .await;
            assert_eq!(
                result.unwrap_err().downcast_ref::<Hold>(),
                Some(&Hold::Exhausted)
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(record_counts(&records.lock().unwrap()),(3,3));
    }

    #[tokio::test]
    async fn missing_prefix_or_changed_cached_proposal_holds_before_observation() {
        let epoch = "c".repeat(64);
        let record = Failure {
            version: 1,
            epoch: epoch.clone(),
            slot: 1,
            proposal_sha256: "d".repeat(64),
            prior_sha256: Some("e".repeat(64)),
            kind: FailureKind::UnexpectedFailure,
            detail: "private".into(),
        };
        let raw = serde_json::to_string(&record).unwrap();
        let result = run(
            &epoch,
            |candidate| Ok((candidate == key(&epoch, 1)).then(|| raw.clone())),
            |_, _| panic!("no write"),
            |_, _, _| -> std::future::Ready<Result<u32>> { panic!("no generation") },
            |_| -> std::future::Ready<Result<()>> { panic!("no observation") },
        )
        .await;
        assert_eq!(
            result.unwrap_err().downcast_ref::<Hold>(),
            Some(&Hold::EvidenceUnknown)
        );
        let record = Failure {
            slot: 0,
            prior_sha256: None,
            ..record
        };
        let raw = serde_json::to_string(&record).unwrap();
        let result = run(
            &epoch,
            |candidate| Ok((candidate == key(&epoch, 0)).then(|| raw.clone())),
            |_, _| panic!("no write"),
            |_, cached, _| async move {
                assert!(cached);
                Ok(0u32)
            },
            |_| -> std::future::Ready<Result<()>> { panic!("no observation") },
        )
        .await;
        assert_eq!(
            result.unwrap_err().downcast_ref::<Hold>(),
            Some(&Hold::EvidenceUnknown)
        );
    }

    #[tokio::test]
    async fn persistence_failure_stops_before_next_correction_and_infrastructure_does_not_consume_slot(
    ) {
        let generated = AtomicUsize::new(0);
        let result = run(
            &"f".repeat(64),
            |_| Ok(None),
            |_, _| anyhow::bail!("scripted persistence failure"),
            |slot, _, _| {
                generated.fetch_add(1, Ordering::SeqCst);
                async move { Ok(slot) }
            },
            |_| async { Err::<(), _>(rejected()) },
        )
        .await;
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("persistence failure"));
        assert_eq!(generated.load(Ordering::SeqCst), 1);
        let result = run(
            &"f".repeat(64),
            |_| Ok(None),
            |_,raw| {
                assert!(serde_json::from_str::<MeasurementIntent>(raw).is_ok(),"infrastructure consumed a semantic correction slot");
                Ok(())
            },
            |slot, _, _| async move { Ok(slot) },
            |_| async {
                Err::<(), _>(
                    BaselineNotRed {
                        detail: "infrastructure".into(),
                        kind: None,
                    }
                    .into(),
                )
            },
        )
        .await;
        let error = result.unwrap_err();
        assert!(
            error.downcast_ref::<BaselineNotRed>().is_some(),
            "private cause is retained"
        );
        assert_eq!(
            error.downcast_ref::<Hold>(),
            Some(&Hold::InfrastructureUnavailable)
        );
    }

    #[test]
    fn strict_lineage_rejects_changed_version_fields_epoch_and_predecessor() {
        let epoch = "1".repeat(64);
        let value = Failure {
            version: 1,
            epoch: epoch.clone(),
            slot: 0,
            proposal_sha256: "2".repeat(64),
            prior_sha256: None,
            kind: FailureKind::UnexpectedFailure,
            detail: "private".into(),
        };
        let original = serde_json::to_value(value).unwrap();
        for (field, replacement) in [
            ("version", serde_json::json!(2)),
            ("slot", serde_json::json!(1)),
            ("epoch", serde_json::json!("3".repeat(64))),
            ("proposal_sha256", serde_json::json!("bad")),
            ("prior_sha256", serde_json::json!("4".repeat(64))),
            ("extra", serde_json::json!(true)),
        ] {
            let mut changed = original.clone();
            changed[field] = replacement;
            assert_eq!(
                decode(&changed.to_string(), &epoch, 0, None)
                    .unwrap_err()
                    .downcast_ref::<Hold>(),
                Some(&Hold::EvidenceUnknown)
            );
        }
        let duplicate = original.to_string().replacen('{', "{\"version\":1,", 1);
        assert!(decode(&duplicate, &epoch, 0, None).is_err());
    }
    #[tokio::test]
    async fn coordinator_uses_real_immutable_workflow_store_after_lease_reopen() {
        use crate::autoship_workflow::{Binding, WorkflowStore};
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("workflow.db");
        let binding = Binding {
            head: "a".repeat(40),
            contract: "b".repeat(64),
            evidence_hash: "c".repeat(64),
        };
        let observations = AtomicUsize::new(0);
        for pass in 0..2 {
            let mut store = WorkflowStore::open(&path).unwrap();
            if pass == 0 {
                store
                    .register("owner/repo#1", "fixture", binding.clone(), 1)
                    .unwrap();
            }
            let now = 2 + pass * 2000;
            let lease = store
                .claim("owner/repo#1", "fixture", now, 1000)
                .unwrap()
                .unwrap();
            let store = Mutex::new(store);
            let result = run(
                &"9".repeat(64),
                |key| {
                    store
                        .lock()
                        .unwrap()
                        .scope_stage_response(&lease, &binding, key, now + 1)
                },
                |key, raw| {
                    store.lock().unwrap().record_scope_stage_response(
                        &lease,
                        &binding,
                        key,
                        raw,
                        now + 1,
                    )
                },
                |slot, cached, _| async move {
                    assert_eq!(cached, pass == 1);
                    Ok(slot)
                },
                |_| {
                    observations.fetch_add(1, Ordering::SeqCst);
                    async { Err::<(), _>(rejected()) }
                },
            )
            .await;
            assert_eq!(
                result.unwrap_err().downcast_ref::<Hold>(),
                Some(&Hold::Exhausted)
            );
            let store = store.into_inner().unwrap();
            assert_eq!(
                store.snapshot("owner/repo#1").unwrap().unwrap().model_calls,
                0
            );
            assert_eq!(
                store
                    .query_connection(|conn| Ok(conn.query_row(
                        "SELECT COUNT(*) FROM scope_stage_response WHERE json_extract(response,'$.kind')='unexpected-failure'",
                        [],
                        |row| row.get::<_, u32>(0)
                    )?))
                    .unwrap(),
                3
            );
        }
        assert_eq!(observations.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn changed_epoch_gets_new_slots_but_unknown_records_never_request_work() {
        let rows = Mutex::new(BTreeMap::<String, String>::new());
        let observations = AtomicUsize::new(0);
        for epoch in ["6".repeat(64), "7".repeat(64)] {
            let result = run(
                &epoch,
                |key| Ok(rows.lock().unwrap().get(key).cloned()),
                |key, raw| {
                    rows.lock().unwrap().insert(key.into(), raw.into());
                    Ok(())
                },
                |slot, cached, _| async move {
                    assert!(!cached);
                    Ok(slot)
                },
                |_| {
                    observations.fetch_add(1, Ordering::SeqCst);
                    async { Err::<(), _>(rejected()) }
                },
            )
            .await;
            assert_eq!(
                result.unwrap_err().downcast_ref::<Hold>(),
                Some(&Hold::Exhausted)
            );
        }
        assert_eq!(observations.load(Ordering::SeqCst), 6);
        assert_eq!(record_counts(&rows.lock().unwrap()),(6,6));
        let result = run(
            &"8".repeat(64),
            |_| Ok(Some("{malformed".into())),
            |_, _| panic!("no write"),
            |_, _, _| -> std::future::Ready<Result<u32>> { panic!("no generation") },
            |_| -> std::future::Ready<Result<()>> { panic!("no observation") },
        )
        .await;
        assert_eq!(
            result.unwrap_err().downcast_ref::<Hold>(),
            Some(&Hold::EvidenceUnknown)
        );
    }

    #[test]
    fn missing_predecessor_field_is_not_equivalent_to_explicit_null() {
        let raw = serde_json::json!({"version":1,"epoch":"a".repeat(64),"slot":0,
            "proposal_sha256":"b".repeat(64),"kind":"unexpected-failure","detail":"private"})
        .to_string();
        assert!(decode(&raw, &"a".repeat(64), 0, None).is_err());
    }
    fn crash_marker(root: &Path, value: &str) {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join("observations"))
            .unwrap();
        writeln!(file, "{value}").unwrap();
        file.sync_all().unwrap();
    }

    async fn crash_fixture(root: &Path, phase: Option<&str>) -> Result<(u32, &'static str)> {
        use crate::autoship_workflow::{Binding, WorkflowStore};
        let path = root.join("workflow.db");
        let binding = Binding {
            head: "a".repeat(40),
            contract: "b".repeat(64),
            evidence_hash: "c".repeat(64),
        };
        let mut store = WorkflowStore::open(&path)?;
        let now = if phase.is_some() {
            store.register("owner/repo#1", "fixture", binding.clone(), 1)?;
            2
        } else {
            2002
        };
        let lease = store
            .claim("owner/repo#1", "fixture", now, 1000)?
            .context("fixture lease unavailable")?;
        let store = Mutex::new(store);
        let result = run(
            &"5".repeat(64),
            |key| {
                store
                    .lock()
                    .unwrap()
                    .scope_stage_response(&lease, &binding, key, now + 1)
            },
            |key, raw| {
                let intent=serde_json::from_str::<MeasurementIntent>(raw).is_ok();
                if (phase==Some("before-failure") && !intent) || (phase==Some("before-intent") && intent) {
                    crash_marker(root, phase.unwrap());
                    std::process::exit(73);
                }
                store.lock().unwrap().record_scope_stage_response(
                    &lease,
                    &binding,
                    key,
                    raw,
                    now + 1,
                )?;
                if (phase==Some("after-failure") && !intent) || (phase==Some("after-intent") && intent) {
                    crash_marker(root, phase.unwrap());
                    std::process::exit(73);
                }
                Ok(())
            },
            |slot, cached, _| {
                let key = amendment::digest(format!("scripted-proposal-v1:{slot}").as_bytes());
                let result = (|| -> Result<u32> {
                    if let Some(raw) = store.lock().unwrap().scope_stage_response(
                        &lease,
                        &binding,
                        &key,
                        now + 1,
                    )? {
                        return Ok(serde_json::from_str(&raw)?);
                    }
                    if cached {
                        return Err(Hold::EvidenceUnknown.into());
                    }
                    crash_marker(root, &format!("model:{slot}")); // scripted boundary only
                    store.lock().unwrap().record_scope_stage_response(
                        &lease,
                        &binding,
                        &key,
                        &slot.to_string(),
                        now + 1,
                    )?;
                    if phase == Some("after-response") {
                        crash_marker(root, "after-response");
                        std::process::exit(73);
                    }
                    Ok(slot)
                })();
                std::future::ready(result)
            },
            |slot| {
                crash_marker(root, &format!("observe:{slot}"));
                std::future::ready(if slot == 0 {
                    Err(rejected())
                } else {
                    Ok("scripted current observation; no production permission")
                })
            },
        )
        .await?;
        if phase == Some("before-positive-persistence") {
            crash_marker(root, "positive-returned-without-persistence");
            std::process::exit(73);
        }
        Ok(result)
    }

    #[tokio::test]
    async fn original_process_crash_reopens_response_failure_and_unpersisted_success_boundaries() {
        const ROOT: &str = "AUTOSHIP_INITIAL_REPAIR_CRASH_ROOT";
        const PHASE: &str = "AUTOSHIP_INITIAL_REPAIR_CRASH_PHASE";
        if let Some(root) = std::env::var_os(ROOT) {
            let phase = std::env::var(PHASE).unwrap();
            crash_fixture(Path::new(&root), Some(&phase)).await.unwrap();
            panic!("fixture did not reach original crash point");
        }
        for phase in [
            "before-intent",
            "after-intent",
            "before-failure",
            "after-failure",
            "after-response",
            "before-positive-persistence",
        ] {
            let root = tempfile::tempdir().unwrap();
            let output=tokio::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact","self_improve::acceptance::initial_repair::tests::original_process_crash_reopens_response_failure_and_unpersisted_success_boundaries"])
                .env(ROOT,root.path()).env(PHASE,phase).output().await.unwrap();
            assert_eq!(
                output.status.code(),
                Some(73),
                "{phase}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let result = crash_fixture(root.path(), None).await.unwrap();
            assert_eq!(result.0, 1);
            let observations = std::fs::read_to_string(root.path().join("observations")).unwrap();
            assert_eq!(
                observations
                    .lines()
                    .filter(|line| *line == "model:0")
                    .count(),
                1
            );
            assert_eq!(
                observations
                    .lines()
                    .filter(|line| *line == "model:1")
                    .count(),
                1
            );
            assert_eq!(
                observations
                    .lines()
                    .filter(|line| *line == "observe:0")
                    .count(),
                if phase == "before-failure" { 2 } else { 1 }
            );
            assert_eq!(
                observations
                    .lines()
                    .filter(|line| *line == "observe:1")
                    .count(),
                if phase == "before-positive-persistence" {
                    2
                } else {
                    1
                }
            );
            if phase == "before-positive-persistence" {
                assert!(observations.contains("positive-returned-without-persistence"));
            } else {
                assert!(observations.lines().any(|line| line == phase));
            }
        }
    }
}
