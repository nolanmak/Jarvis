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
}
impl Hold {
    pub(super) fn reason(self) -> &'static str {
        match self {
            Self::Exhausted => "acceptance_verifier_repair_exhausted",
            Self::EvidenceUnknown => "acceptance_verifier_evidence_unknown",
            Self::ContractInvalid => "acceptance_contract_invalid",
        }
    }
    pub(super) fn action(self) -> &'static str {
        match self {
            Self::EvidenceUnknown => "await_original_evidence_recovery",
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
    for slot in 0..LIMIT {
        let cached = (slot as usize) < failures.len();
        let previous = slot
            .checked_sub(1)
            .map(|prior| failures[prior as usize].0.clone());
        let proposal = generate(slot, cached, previous).await?;
        let proposal_sha256 = amendment::digest(&serde_json::to_vec(&proposal)?);
        if cached {
            if proposal_sha256 != failures[slot as usize].0.proposal_sha256 {
                return Err(Hold::EvidenceUnknown.into());
            }
            continue;
        }
        match observe(proposal.clone()).await {
            Ok(baseline) => return Ok((proposal, baseline)),
            Err(error) => {
                let Some(failed) = error.downcast_ref::<BaselineNotRed>() else {
                    return Err(error);
                };
                let Some(kind) = failed.kind else {
                    return Err(error);
                }; // infrastructure
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
            "positive evidence requires real re-observation"
        );
        assert_eq!(records.lock().unwrap().len(), 2);
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
        assert_eq!(records.lock().unwrap().len(), 3);
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
            |_, _| panic!("infrastructure is not invalid candidate"),
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
        assert!(result
            .unwrap_err()
            .downcast_ref::<BaselineNotRed>()
            .is_some());
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
                        "SELECT COUNT(*) FROM scope_stage_response",
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
        assert_eq!(rows.lock().unwrap().len(), 6);
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
}
