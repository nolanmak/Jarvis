//! Bounded controller observations, never admission or live-readiness proof.
use augmentagent_channel_core::run_resources::Readiness;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

const LIMIT: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawObservation")]
pub struct Observation {
    schema_version: u8,
    status: String,
    refusal: Option<Refusal>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawObservation {
    schema_version: u8,
    status: String,
    refusal: Option<Refusal>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Refusal {
    phase: String,
    exhausted: bool,
    refusal_code: String,
    #[serde(default)]
    facts: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    failure_class: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    errno: Option<u64>,
}

fn fields(code: &str) -> Option<&'static [&'static str]> {
    Some(match code {
        "host_headroom" => &[
            "requested_bytes",
            "reservation_slack_bytes",
            "margin_bytes",
            "required_bytes",
            "available_bytes",
        ],
        "aggregate_reservations" => &[
            "requested_bytes",
            "reserved_bytes",
            "current_bytes",
            "reclaimable_bytes",
            "limit_bytes",
        ],
        "ancestor_headroom" => &[
            "requested_bytes",
            "reservation_slack_bytes",
            "current_bytes",
            "limit_bytes",
        ],
        "declared_ceiling" => &[
            "requested_bytes",
            "limit_bytes",
            "requested_pids",
            "limit_pids",
            "requested_cpu_percent",
            "limit_cpu_quota",
            "limit_cpu_period",
        ],
        "invalid_policy" => &["memory_bytes", "pids", "cpu_percent", "minimum", "maximum"],
        "admission_setup_failed" => &[],
        _ => return None,
    })
}

impl TryFrom<RawObservation> for Observation {
    type Error = &'static str;
    fn try_from(raw: RawObservation) -> Result<Self, Self::Error> {
        if raw.schema_version != 1
            || !matches!(
                raw.status.as_str(),
                "declaration-invalid"
                    | "declaration-exceeds-service"
                    | "delegation-unavailable"
                    | "inspection-busy"
                    | "capacity-exhausted"
                    | "inspection-unavailable"
            )
        {
            return Err("invalid resource observation");
        }
        if let Some(refusal) = &raw.refusal {
            let allowed = fields(&refusal.refusal_code).ok_or("unknown resource refusal")?;
            if refusal.phase != "admission_failed"
                || !refusal.exhausted
                || refusal
                    .facts
                    .keys()
                    .any(|key| !allowed.contains(&key.as_str()))
            {
                return Err("invalid resource refusal");
            }
            if refusal.refusal_code != "admission_setup_failed"
                && (refusal.errno.is_some() || refusal.failure_class.is_some())
            {
                return Err("unexpected resource failure metadata");
            }
            if refusal.errno.is_some_and(|errno| errno > 4095)
                || refusal.failure_class.as_deref().is_some_and(|class| {
                    !matches!(
                        class,
                        "RuntimeError"
                            | "ValueError"
                            | "TypeError"
                            | "JSONDecodeError"
                            | "OSError"
                            | "FileNotFoundError"
                            | "PermissionError"
                            | "BlockingIOError"
                            | "NotADirectoryError"
                            | "TimeoutError"
                            | "BoundaryFailure"
                    )
                })
            {
                return Err("invalid resource failure metadata");
            }
        }
        let observation = Self {
            schema_version: raw.schema_version,
            status: raw.status,
            refusal: raw.refusal,
        };
        if serde_json::to_vec(&observation)
            .map_err(|_| "invalid resource observation")?
            .len()
            > LIMIT
        {
            return Err("resource observation exceeds bound");
        }
        Ok(observation)
    }
}

impl Observation {
    /// Copy only the original controller's allowlisted facts. Malformed known
    /// fields invalidate this optional diagnostic, never change admission.
    pub fn capture(readiness: &Readiness) -> Option<Self> {
        if readiness.permits_attempt || readiness.live_verified {
            return None;
        }
        let raw = readiness.observation.as_ref()?;
        if raw["status"].as_str()? != readiness.status
            || raw["permits_attempt"] != false
            || raw["live_verified"] != false
        {
            return None;
        }
        let refusal = match raw.get("refusal") {
            None => Value::Null,
            Some(value) => {
                let code = value["refusal_code"].as_str()?;
                let allowed = fields(code)?;
                let mut facts = BTreeMap::new();
                if let Some(value) = value.get("facts") {
                    let object = value.as_object()?;
                    for key in allowed {
                        if let Some(value) = object.get(*key) {
                            facts.insert(*key, value.as_u64()?);
                        }
                    }
                }
                let mut result = json!({"phase":value["phase"], "exhausted":value["exhausted"],
                    "refusal_code":code, "facts":facts});
                if code == "admission_setup_failed" {
                    for key in ["failure_class", "errno"] {
                        if let Some(value) = value.get(key) {
                            result[key] = value.clone();
                        }
                    }
                }
                result
            }
        };
        serde_json::from_value(
            json!({"schema_version":1, "status":readiness.status, "refusal":refusal}),
        )
        .ok()
    }

    pub fn decode(raw: &str) -> anyhow::Result<Self> {
        anyhow::ensure!(raw.len() <= LIMIT, "resource observation exceeds bound");
        serde_json::from_str(raw).map_err(|_| anyhow::anyhow!("invalid resource observation"))
    }

    pub fn identity(&self) -> Value {
        let mut value = serde_json::to_value(self).expect("bounded observation serializes");
        if let Some(facts) = value
            .get_mut("refusal")
            .and_then(|value| value.get_mut("facts"))
            .and_then(Value::as_object_mut)
        {
            facts.retain(|key, _| {
                !matches!(
                    key.as_str(),
                    "available_bytes"
                        | "reserved_bytes"
                        | "current_bytes"
                        | "reclaimable_bytes"
                        | "reservation_slack_bytes"
                )
            });
        }
        value
    }
}

impl std::fmt::Display for Observation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("resource-preflight-observation")
    }
}
impl std::error::Error for Observation {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttemptObservation {
    pub selected_at_ms: i64,
    pub finished_at_ms: i64,
    pub observation: Observation,
}

impl AttemptObservation {
    pub fn render(&self) -> String {
        format!(
            "Last observed resource refusal (attempt {} to {} ms UTC): {}",
            self.selected_at_ms,
            self.finished_at_ms,
            serde_json::to_string(&self.observation).expect("bounded observation serializes")
        )
    }
}

pub fn render_status(status: &Value) -> Option<String> {
    let observation: AttemptObservation =
        serde_json::from_value(status["resource_refusal"].clone()).ok()?;
    Some(observation.render())
}

#[cfg(test)]
mod tests {
    use super::super::{
        submission::{AttemptOutcome, Milestone, Request, Target},
        WorkflowStore,
    };
    use super::*;
    use serde_json::json;

    fn readiness() -> Readiness {
        Readiness {
            status: "capacity-exhausted".into(),
            permits_attempt: false,
            live_verified: false,
            policy: None,
            observation: Some(
                json!({"status":"capacity-exhausted", "permits_attempt":false, "live_verified":false,
                "refusal":{"phase":"admission_failed", "exhausted":true, "refusal_code":"host_headroom",
                    "facts":{"requested_bytes":6442450944_u64, "required_bytes":6710886400_u64,
                        "margin_bytes":268435456, "available_bytes":6000000000_u64, "reservation_slack_bytes":0}}}),
            ),
        }
    }
    fn request() -> Request {
        Request {
            repository: "owner/repo".into(),
            target: Target::Pr(42),
            provider: None,
            until: Milestone::Merged,
        }
    }
    fn selected(path: &std::path::Path) -> (WorkflowStore, String) {
        let mut store = WorkflowStore::open(path).unwrap();
        store
            .configure_delivery(
                "owner/repo",
                "github:owner",
                super::super::delivery::Destination {
                    bot: 11,
                    owner: 22,
                    channel: 33,
                },
                0,
            )
            .unwrap();
        let id = store
            .submit_authorized("request", "github:owner", &request(), 1)
            .unwrap()
            .workflow;
        assert_eq!(
            store.next_command_job("owner/repo", 2).unwrap().as_deref(),
            Some(id.as_str())
        );
        (store, id)
    }
    fn finish(store: &WorkflowStore, id: &str, at: i64, observation: &Observation) -> bool {
        store
            .finish_command_attempt_observed(
                id,
                at,
                AttemptOutcome::Deferred,
                "execution-readiness-blocked",
                at + 1,
                Some(observation),
            )
            .unwrap()
    }

    #[test]
    fn original_controller_facts_are_bounded_and_private_fields_never_serialize() {
        let mut report = readiness();
        report.policy = Some(json!({"secret":"PRIVATE policy"}));
        report.observation.as_mut().unwrap()["refusal"]["facts"]["private_path"] =
            "PRIVATE path".into();
        report.observation.as_mut().unwrap()["refusal"]["provider_output"] = "PRIVATE prose".into();
        let observation = Observation::capture(&report).unwrap();
        let raw = serde_json::to_string(&observation).unwrap();
        assert!(!raw.contains("PRIVATE"));
        assert_eq!(Observation::decode(&raw).unwrap(), observation);
        assert_eq!(
            serde_json::to_value(&observation).unwrap()["refusal"]["facts"]["required_bytes"],
            6710886400_u64
        );
        for value in [
            json!(-1),
            json!(1.5),
            json!(true),
            json!("PRIVATE"),
            json!(18446744073709551616.0),
        ] {
            let mut invalid = readiness();
            invalid.observation.as_mut().unwrap()["refusal"]["facts"]["available_bytes"] = value;
            assert!(Observation::capture(&invalid).is_none());
        }
        for field in ["status", "permits_attempt", "live_verified"] {
            let mut invalid = readiness();
            invalid.observation.as_mut().unwrap()[field] = "PRIVATE".into();
            assert!(Observation::capture(&invalid).is_none(), "{field}");
        }
        for field in ["phase", "exhausted", "refusal_code"] {
            let mut invalid = readiness();
            invalid.observation.as_mut().unwrap()["refusal"][field] = "PRIVATE".into();
            assert!(Observation::capture(&invalid).is_none(), "{field}");
        }
        for field in ["schema_version", "unknown"] {
            let mut invalid = serde_json::to_value(&observation).unwrap();
            invalid[field] = json!(99);
            assert!(
                Observation::decode(&invalid.to_string()).is_err(),
                "{field}"
            );
        }
        let mut invalid = serde_json::to_value(&observation).unwrap();
        invalid["refusal"]["facts"]["unknown"] = json!(1);
        assert!(Observation::decode(&invalid.to_string()).is_err());
        assert!(Observation::decode(&"x".repeat(1025)).is_err());
    }

    #[test]
    fn every_controller_code_has_its_own_numeric_fact_allowlist() {
        for code in [
            "host_headroom",
            "aggregate_reservations",
            "ancestor_headroom",
            "declared_ceiling",
            "invalid_policy",
            "admission_setup_failed",
        ] {
            let mut report = readiness();
            report.observation.as_mut().unwrap()["refusal"] = json!({"phase":"admission_failed", "exhausted":true,
                "refusal_code":code,"facts":fields(code).unwrap().iter().map(|field|((*field).to_string(),json!(u64::MAX))).collect::<serde_json::Map<_,_>>()});
            if code == "admission_setup_failed" {
                report.observation.as_mut().unwrap()["refusal"]["failure_class"] =
                    "PermissionError".into();
                report.observation.as_mut().unwrap()["refusal"]["errno"] = 13.into();
            }
            let observation = Observation::capture(&report).unwrap();
            assert!(serde_json::to_vec(&observation).unwrap().len() <= 1024);
            let attempt = AttemptObservation {
                selected_at_ms: 2,
                finished_at_ms: 3,
                observation,
            };
            assert!(attempt.render().len() < 900);
        }
        let mut report = readiness();
        report.observation.as_mut().unwrap()["refusal"] = json!({"phase":"admission_failed","exhausted":true,
            "refusal_code":"admission_setup_failed","errno":4096});
        assert!(Observation::capture(&report).is_none());
        report.observation.as_mut().unwrap()["refusal"]["errno"] = 13.into();
        report.observation.as_mut().unwrap()["refusal"]["failure_class"] = "PRIVATE".into();
        assert!(Observation::capture(&report).is_none());
    }

    #[test]
    fn replacement_is_fenced_and_unchanged_blockers_refresh_state_without_resending() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workflow.db");
        let (mut store, id) = selected(&path);
        let first = Observation::capture(&readiness()).unwrap();
        assert!(finish(&store, &id, 2, &first));
        let events = store.progress_events(&id).unwrap();
        let old = events.last().unwrap().clone();
        let origin = super::super::discord_ingress::DiscordOrigin {
            bot: 11,
            owner: 22,
            channel: 33,
            message: 44,
        };
        let (_, content) = store
            .claim_progress(&origin, &id, old.sequence, 4)
            .unwrap()
            .unwrap();
        assert!(content.contains("host_headroom") && content.contains("6000000000"));
        assert!(content.chars().count() <= 1900);
        assert!(store
            .next_command_job("owner/repo", 300002)
            .unwrap()
            .is_none());
        assert_eq!(
            store
                .next_command_job("owner/repo", 300002)
                .unwrap()
                .as_deref(),
            Some(id.as_str())
        );
        assert!(store
            .command_attempt(&id)
            .unwrap()
            .unwrap()
            .resource_refusal
            .is_none());
        assert!(
            !finish(&store, &id, 2, &first),
            "stale result changed replacement selection"
        );
        let mut report = readiness();
        report.observation.as_mut().unwrap()["refusal"]["facts"]["available_bytes"] =
            6100000000_u64.into();
        let second = Observation::capture(&report).unwrap();
        assert!(finish(&store, &id, 300002, &second));
        assert!(
            !finish(&store, &id, 300002, &first),
            "duplicate completion overwrote observation"
        );
        assert_eq!(
            store.progress_events(&id).unwrap(),
            events,
            "usage fluctuation sent another event"
        );
        let (_, status) = crate::autoship_commands::status(&path, &id, 300004, 5);
        assert_eq!(
            status["resource_refusal"]["observation"]["refusal"]["facts"]["available_bytes"],
            6100000000_u64
        );
        assert!(render_status(&status).unwrap().contains("6100000000"));
        let latest: String = store
            .conn
            .query_row(
                "SELECT payload FROM workflow_progress_state WHERE workflow=?1",
                [&id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(latest.contains("6100000000"));
        let uncertain = store.uncertain_progress(11, 22).unwrap();
        assert_eq!(
            uncertain[0].4, content,
            "already dispatched content mutated"
        );
        store
            .confirm_progress(&origin, &id, old.sequence, 55, 300005)
            .unwrap();
        assert!(store.pending_progress(11, 22).unwrap().is_empty());
        assert!(store.snapshot(&id).unwrap().is_none());
        assert_eq!(store.merge_allowance(300005, 5).unwrap().available, 5);
        drop(store);
        let reopened = WorkflowStore::open_read_only(&path).unwrap().unwrap();
        assert_eq!(
            reopened
                .command_attempt(&id)
                .unwrap()
                .unwrap()
                .resource_refusal,
            Some(second)
        );
    }

    #[test]
    fn projection_failure_rolls_back_completion_and_observation_together() {
        let dir = tempfile::tempdir().unwrap();
        let (store, id) = selected(&dir.path().join("workflow.db"));
        store.conn.execute_batch("CREATE TRIGGER fail_resource_projection BEFORE INSERT ON workflow_progress_state BEGIN SELECT RAISE(ABORT,'fixture projection refusal'); END;").unwrap();
        let observation = Observation::capture(&readiness()).unwrap();
        assert!(store
            .finish_command_attempt_observed(
                &id,
                2,
                AttemptOutcome::Deferred,
                "execution-readiness-blocked",
                3,
                Some(&observation)
            )
            .is_err());
        let attempt = store.command_attempt(&id).unwrap().unwrap();
        assert_eq!(attempt.outcome, AttemptOutcome::Selected);
        assert!(attempt.resource_refusal.is_none());
        assert!(attempt.finished_at_ms.is_none());
        let rows: u64 = store
            .conn
            .query_row("SELECT count(*) FROM command_resource_refusal", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[test]
    fn legacy_read_only_store_stays_unchanged_then_write_open_installs_receipts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workflow.db");
        let (store, id) = selected(&path);
        store
            .conn
            .execute_batch("DROP TABLE command_resource_refusal;")
            .unwrap();
        drop(store);
        let before = std::fs::read(&path).unwrap();
        let read = WorkflowStore::open_read_only(&path).unwrap().unwrap();
        assert!(read
            .command_attempt(&id)
            .unwrap()
            .unwrap()
            .resource_refusal
            .is_none());
        let (_, status) = crate::autoship_commands::status(&path, &id, 4, 5);
        assert!(status["resource_refusal"].is_null());
        drop(read);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let store = WorkflowStore::open(&path).unwrap();
        assert!(finish(
            &store,
            &id,
            2,
            &Observation::capture(&readiness()).unwrap()
        ));
        assert!(store
            .command_attempt(&id)
            .unwrap()
            .unwrap()
            .resource_refusal
            .is_some());
        let mut old =
            serde_json::to_value(store.progress_events(&id).unwrap().last().unwrap()).unwrap();
        old.as_object_mut().unwrap().remove("resource_refusal");
        let restored: super::super::progress::Progress = serde_json::from_value(old).unwrap();
        assert!(restored.resource_refusal.is_none());
    }
    #[test]
    fn changed_demand_emits_a_new_blocker_after_numeric_refresh_was_deduplicated() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, id) = selected(&dir.path().join("workflow.db"));
        assert!(finish(
            &store,
            &id,
            2,
            &Observation::capture(&readiness()).unwrap()
        ));
        let count = store.progress_events(&id).unwrap().len();
        assert!(store
            .next_command_job("owner/repo", 300002)
            .unwrap()
            .is_none());
        assert!(store
            .next_command_job("owner/repo", 300002)
            .unwrap()
            .is_some());
        let mut report = readiness();
        report.observation.as_mut().unwrap()["refusal"]["facts"]["requested_bytes"] =
            6442450945_u64.into();
        assert!(finish(
            &store,
            &id,
            300002,
            &Observation::capture(&report).unwrap()
        ));
        assert_eq!(store.progress_events(&id).unwrap().len(), count + 1);
    }

    #[test]
    fn maximal_valid_identity_and_numeric_facts_fit_private_delivery() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = WorkflowStore::open(&dir.path().join("workflow.db")).unwrap();
        let repository = format!("{}/{}", "a".repeat(127), "b".repeat(128));
        let request = Request {
            repository: repository.clone(),
            target: Target::Pr(i64::MAX as u64),
            provider: None,
            until: Milestone::Merged,
        };
        store
            .configure_delivery(
                &repository,
                "github:owner",
                super::super::delivery::Destination {
                    bot: 11,
                    owner: 22,
                    channel: 33,
                },
                0,
            )
            .unwrap();
        let id = store
            .submit_authorized("maximal", "github:owner", &request, 1)
            .unwrap()
            .workflow;
        store
            .next_command_job(&repository, i64::MAX - 1)
            .unwrap()
            .unwrap();
        let mut report = readiness();
        report.observation.as_mut().unwrap()["refusal"] = json!({"phase":"admission_failed","exhausted":true,"refusal_code":"declared_ceiling",
            "facts":fields("declared_ceiling").unwrap().iter().map(|field|((*field).to_string(),json!(u64::MAX))).collect::<serde_json::Map<_,_>>()});
        assert!(finish(
            &store,
            &id,
            i64::MAX - 1,
            &Observation::capture(&report).unwrap()
        ));
        let event = store.progress_events(&id).unwrap().pop().unwrap();
        let origin = super::super::discord_ingress::DiscordOrigin {
            bot: 11,
            owner: 22,
            channel: 33,
            message: 44,
        };
        let (_, content) = store
            .claim_progress(&origin, &id, event.sequence, i64::MAX)
            .unwrap()
            .unwrap();
        assert!(content.contains("declared_ceiling") && content.contains("18446744073709551615"));
        assert!(content.chars().count() <= 1900);
    }
    #[test]
    fn commands_without_resource_observations_keep_the_legacy_progress_fingerprint() {
        use sha2::{Digest, Sha256};
        let dir = tempfile::tempdir().unwrap();
        let (store, id) = selected(&dir.path().join("workflow.db"));
        let event = store.progress_events(&id).unwrap().pop().unwrap();
        let expected = format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&(
                    &event.kind,
                    event.stage,
                    &event.reason,
                    &event.next_action,
                    &event.evidence_digest,
                    &event.actual_provider,
                    &event.actual_provider_role
                ))
                .unwrap()
            )
        );
        let actual: String = store
            .conn
            .query_row(
                "SELECT fingerprint FROM workflow_progress_state WHERE workflow=?1",
                [&id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            actual, expected,
            "schema upgrade would resend an unchanged existing blocker"
        );
    }

    #[test]
    fn corrupt_stored_observation_cannot_leak_untrusted_field_names_in_status_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workflow.db");
        let (store, id) = selected(&path);
        assert!(finish(
            &store,
            &id,
            2,
            &Observation::capture(&readiness()).unwrap()
        ));
        let mut invalid =
            serde_json::to_value(Observation::capture(&readiness()).unwrap()).unwrap();
        invalid["PRIVATE_untrusted_field"] = json!(1);
        store
            .conn
            .execute(
                "UPDATE command_resource_refusal SET payload=?2 WHERE workflow=?1",
                rusqlite::params![id, invalid.to_string()],
            )
            .unwrap();
        let (code, status) = crate::autoship_commands::status(&path, &id, 4, 5);
        assert_eq!(code, 4);
        assert!(
            !status.to_string().contains("PRIVATE"),
            "invalid stored receipt leaked its rejected field: {status}"
        );
    }
}
