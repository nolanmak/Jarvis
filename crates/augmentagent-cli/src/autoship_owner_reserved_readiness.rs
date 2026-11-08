//! Saved body-state data only. Native observations and workflow authority stay
//! borrowed from their actual producers; this format cannot recreate either.
use super::*;
use augmentagent_channel_core::native_vm::{ownership::Authority, PreparedLaunchIntent};
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
pub(crate) enum ReadinessBodyState {
    ReservationAbsent,
    ReservedModelUnadmitted(ReservedInputRecord),
    ReservedModelAdmittedNativeAbsent(ReservedInputRecord),
    ReservedSqlBoundNativeUnbound(ReservedInputRecord),
    ReservedNativeBound(ReservedInputRecord),
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReservedInputRecord {
    original: OriginalBodyInputHistory,
    attempt_lineage: Vec<OriginalAttempt>,
    held_attempt: AttemptReference,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AttemptReference {
    intent_id: String,
    launch_receipt: augmentagent_channel_core::audit_chain::AppendReceipt,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, tag = "kind", rename_all = "snake_case")]
enum OriginalAttempt {
    OriginalReservation {
        reservation: HistoryReference,
        prepared: PreparedSetup,
        model: Option<ModelSlot>,
        native: Option<NativeSlot>,
    },
}
fn sha(value: &serde_json::Value) -> bool {
    value
        .as_str()
        .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| matches!(b,b'0'..=b'9'|b'a'..=b'f')))
}
fn object(value: &serde_json::Value, fields: usize) -> bool {
    value.as_object().is_some_and(|o| o.len() == fields)
}
fn initial_attempt(history: &OriginalBodyInputHistory) -> OriginalAttempt {
    OriginalAttempt::OriginalReservation {
        reservation: history.reservation.clone(),
        prepared: history.setup.clone(),
        model: history.model.clone(),
        native: history.native.clone(),
    }
}
fn selected(history: &OriginalBodyInputHistory) -> AttemptReference {
    AttemptReference {
        intent_id: history.setup.intent.intent_id.clone(),
        launch_receipt: history.setup.launch_receipt.clone(),
    }
}
fn closure(value: &serde_json::Value, intent: &PreparedLaunchIntent) -> Result<()> {
    ensure!(
        value["version"] == 1
            && value["source"]["owner"] == serde_json::to_value(&intent.source_owner)?
            && value["source"]["audit_path"] == serde_json::to_value(&intent.source_audit_path)?
            && object(&value["source"], 2),
        "reserved readiness writer closure source changed"
    );
    match value["kind"].as_str() {
        Some("completed_native_transfer") => {
            let id = value["transfer_id"]
                .as_str()
                .context("reserved readiness transfer closure missing")?;
            ensure!(
                object(value, 6)
                    && uuid::Uuid::parse_str(id)?.to_string() == id
                    && sha(&value["request_sha256"])
                    && sha(&value["receipt_sha256"]),
                "reserved readiness transfer closure changed"
            );
        }
        Some("retired_native_owner") => ensure!(
            object(value, 3),
            "reserved readiness retirement closure changed"
        ),
        _ => bail!("reserved readiness writer has no permanent closure"),
    }
    Ok(())
}
impl ReadinessBodyState {
    fn payload(&self) -> Result<&ReservedInputRecord> {
        match self {
            Self::ReservedModelUnadmitted(v)
            | Self::ReservedModelAdmittedNativeAbsent(v)
            | Self::ReservedSqlBoundNativeUnbound(v)
            | Self::ReservedNativeBound(v) => Ok(v),
            Self::ReservationAbsent => {
                bail!("reserved readiness requires its original reservation")
            }
        }
    }
    pub(crate) fn from_reserved(read: &ReservedBodyNonreleaseRead<'_>) -> Result<Self> {
        let history = read.history();
        let payload = ReservedInputRecord {
            original: history.clone(),
            attempt_lineage: vec![initial_attempt(history)],
            held_attempt: selected(history),
        };
        let native = read.native_evidence();
        let result = match (&history.state, native["attempts"][0]["kind"].as_str()) {
            (SqlState::ReservedModelUnadmitted, Some("unbound")) => {
                Self::ReservedModelUnadmitted(payload)
            }
            (SqlState::ReservedModelAdmittedNativeAbsent, Some("unbound")) => {
                Self::ReservedModelAdmittedNativeAbsent(payload)
            }
            (SqlState::ReservedSqlBound, Some("closed_before_binding")) => {
                Self::ReservedSqlBoundNativeUnbound(payload)
            }
            (SqlState::ReservedSqlBound, Some("bound")) => Self::ReservedNativeBound(payload),
            _ => bail!("reserved readiness SQL/native body state differs"),
        };
        result.validate_native(
            native,
            &serde_json::from_value(native["namespace"].clone())?,
        )?;
        Ok(result)
    }
    fn validate_native(&self, native: &serde_json::Value, namespace: &Authority) -> Result<()> {
        let payload = self.payload()?;
        let h = &payload.original;
        // Until recovered-model/input projections exist, only this explicit
        // original attempt can be represented. Never select a newest row or
        // silently discard another native writer from the core census.
        ensure!(
            payload.attempt_lineage == vec![initial_attempt(h)]
                && payload.held_attempt == selected(h),
            "reserved readiness attempt lineage or explicit selection changed"
        );
        let expected_input = h.native.as_ref().map(|v| &v.admission.reference);
        let (state, disposition) = match self {
            Self::ReservedModelUnadmitted(_) => (SqlState::ReservedModelUnadmitted, "unbound"),
            Self::ReservedModelAdmittedNativeAbsent(_) => {
                (SqlState::ReservedModelAdmittedNativeAbsent, "unbound")
            }
            Self::ReservedSqlBoundNativeUnbound(_) => {
                (SqlState::ReservedSqlBound, "closed_before_binding")
            }
            Self::ReservedNativeBound(_) => (SqlState::ReservedSqlBound, "bound"),
            Self::ReservationAbsent => unreachable!(),
        };
        ensure!(
            h.state == state
                && match h.state {
                    SqlState::ReservedModelUnadmitted => h.model.is_none() && h.native.is_none(),
                    SqlState::ReservedModelAdmittedNativeAbsent =>
                        h.model.is_some() && h.native.is_none(),
                    SqlState::ReservedSqlBound => h.model.is_some() && h.native.is_some(),
                },
            "reserved readiness original model/native slot changed"
        );
        ensure!(
            object(native, 9)
                && native["version"] == 1
                && native["kind"] == "native_body_input_nonrelease"
                && native["request_id"] == h.request_id
                && namespace.owner.workflow_id == h.workflow
                && native["namespace"] == serde_json::to_value(namespace)?
                && native["original_binding_sha256"]
                    == h.setup.intent.original_store_binding_sha256
                && sha(&native["native_metadata_sha256"])
                && sha(&native["writer_revocation_sha256"]),
            "reserved readiness native input evidence changed"
        );
        let attempts = native["attempts"]
            .as_array()
            .context("reserved readiness native attempts missing")?;
        ensure!(
            attempts.len() == 1,
            "reserved readiness native lineage differs"
        );
        let a = &attempts[0];
        ensure!(
            object(a, 7)
                && a["kind"] == disposition
                && a["launch_intent"] == serde_json::to_value(&h.setup.intent)?
                && a["launch_receipt"] == serde_json::to_value(&h.setup.launch_receipt)?
                && a["resource_receipt"] == serde_json::to_value(&h.setup.resource_receipt)?
                && a["expected_owner_input"] == serde_json::to_value(expected_input)?
                && a["observed_owner_input"]
                    == if disposition == "bound" {
                        serde_json::to_value(expected_input)?
                    } else {
                        serde_json::Value::Null
                    },
            "reserved readiness selected native attempt changed"
        );
        closure(&a["closure"], &h.setup.intent)?;
        let setup = native["unassociated_setup_launches"]
            .as_array()
            .context("reserved readiness setup census missing")?;
        ensure!(
            setup.len() <= 4096,
            "reserved readiness setup census exceeds bound"
        );
        let mut seen = std::collections::BTreeSet::from([h.setup.intent.intent_id.clone()]);
        for writer in setup {
            let intent: PreparedLaunchIntent =
                serde_json::from_value(writer["launch_intent"].clone())?;
            intent.validate()?;
            ensure!(
                object(writer, 10)
                    && writer["owner_input"].is_null()
                    && writer["release_attempted"] == false
                    && intent.source_owner.workflow_id == h.workflow
                    && seen.insert(intent.intent_id.clone()),
                "reserved readiness unassociated writer census changed"
            );
            closure(&writer["closure"], &intent)?;
        }
        Ok(())
    }
    /// Immutable joins only. Current authorization, publication/cycle ancestry,
    /// fresh whole-native proof and the consuming transaction remain separate.
    pub(crate) fn validate_saved(
        &self,
        conn: &Connection,
        workflow: &str,
        request: &str,
        reservation: (i64, &str),
        preparation: &OwnerPreparationBinding,
        provider: Option<submission::Provider>,
        native: &serde_json::Value,
        namespace: &Authority,
        before: i64,
    ) -> Result<()> {
        let payload = self.payload()?;
        let h = &payload.original;
        ensure!(
            h.workflow == workflow
                && h.request_id == request
                && h.preparation == *preparation
                && provider.is_none_or(|provider| provider == h.provider)
                && (h.reservation.sequence, h.reservation.sha256.as_str()) == reservation
                && reservation.0 < before
                && h.model.as_ref().is_none_or(
                    |m| m.admission.sequence < before && m.charge.event.sequence < before
                )
                && h.native.as_ref().is_none_or(|n| n.record.sequence < before),
            "reserved readiness original input snapshot changed"
        );
        ensure!(
            read_recorded_input_slots(conn, workflow, request)?.as_ref() == Some(h),
            "reserved readiness immutable SQL history changed"
        );
        self.validate_native(native, namespace)
    }
}
