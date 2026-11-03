//! C8 (D10) — what the notice buttons do: Undo (remove the guest the plan
//! added, unless they accepted), Pause this person, Pause all, and Confirm
//! a pending trusted contact. Every control is idempotent.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use augmentagent_approval_discord::autonomy::{AutonomyControl, STEP_ADD_GUEST};
use augmentagent_channel_calendar::{EventRef, GuestOutcome};
use augmentagent_store::autonomy::{PlanStatus, StepStatus};
use augmentagent_store::Store;

use crate::calendar::CalendarOps;

pub struct Controls {
    pub store: Arc<Store>,
    pub calendar: Option<Arc<dyn CalendarOps>>,
    pub config_path: PathBuf,
}

impl Controls {
    fn now() -> i64 {
        chrono::Utc::now().timestamp_millis()
    }
}

#[async_trait]
impl AutonomyControl for Controls {
    async fn undo(&self, plan_id: i64) -> String {
        let store = &self.store;
        let Ok(Some(plan)) = store.autonomous_plan(plan_id) else {
            return format!("No autonomous plan {plan_id}.");
        };
        let steps = store.autonomous_steps(plan_id).unwrap_or_default();
        let Some(step) = steps.iter().find(|s| s.kind == STEP_ADD_GUEST) else {
            return "Nothing to undo: this plan only sent a message, and messages can't be unsent."
                .into();
        };
        match step.status {
            StepStatus::Undone => return "Already undone.".into(),
            StepStatus::Executed => {}
            other => return format!("Nothing to undo: the guest step is {other}."),
        }
        let undo: serde_json::Value = step
            .undo_json
            .as_deref()
            .and_then(|u| serde_json::from_str(u).ok())
            .unwrap_or_default();
        let field = |k: &str| {
            undo.get(k)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        };
        let event = EventRef {
            account: field("account"),
            calendar_id: field("calendar_id"),
            event_id: field("event_id"),
        };
        let guest = field("email");
        if event.account.is_empty() || event.event_id.is_empty() || guest.is_empty() {
            return "Can't undo: the plan did not record which event and guest to remove.".into();
        }
        let Some(cal) = &self.calendar else {
            return "Can't undo here: the calendar client is not connected in this process.".into();
        };
        let mark_undone = || {
            let now = Self::now();
            let _ = store.set_autonomous_step_status(step.id, StepStatus::Undone, None, now);
            let all = store.autonomous_steps(plan_id).unwrap_or_default();
            let reversible_left = all
                .iter()
                .any(|s| s.kind == STEP_ADD_GUEST && s.status == StepStatus::Executed);
            if !reversible_left {
                let _ = store.set_autonomous_plan_status(plan.id, PlanStatus::Undone, now);
            }
        };
        match cal.remove_guest(&event, &guest).await {
            Ok(GuestOutcome::Removed { .. }) => {
                mark_undone();
                format!("Undone: removed {guest} from the event. Google sends them a cancellation.")
            }
            Ok(GuestOutcome::Noop) => {
                mark_undone();
                format!("Already undone: {guest} is not on the event.")
            }
            Ok(GuestOutcome::AlreadyAccepted) => {
                "Not undone: they already accepted. Remove them in Calendar if needed.".into()
            }
            Ok(GuestOutcome::NotOrganizer) => {
                "Not undone: the owner's account no longer organizes that event.".into()
            }
            Ok(GuestOutcome::Unconfirmed(m)) => {
                format!(
                    "Undo outcome unknown ({m}). Check the event in Calendar before trying again."
                )
            }
            Ok(other) => format!("Not undone: calendar answered {other:?}."),
            Err(e) => format!("Undo failed: {e}"),
        }
    }

    async fn pause(&self, slug: &str) -> String {
        match self.store.pause_contact(slug, Self::now()) {
            Ok(true) => format!(
                "Paused autonomy for {slug}. `augmentagent trust resume {slug}` resumes it."
            ),
            Ok(false) => format!("Autonomy for {slug} was already paused."),
            Err(e) => format!("Pause failed: {e}"),
        }
    }

    async fn pause_all(&self) -> String {
        match self.store.set_autonomy_global_pause(true, Self::now()) {
            Ok(()) => "Paused all autonomy. `augmentagent trust resume-all` resumes it.".into(),
            Err(e) => format!("Pause all failed: {e}"),
        }
    }

    async fn confirm(&self, slug: &str, entry_hash_prefix: &str) -> String {
        let report = crate::config::load_config(&self.config_path);
        let Some(contact) = report.contact(slug) else {
            return format!(
                "No valid contact `{slug}` in the trusted-contacts config; nothing confirmed."
            );
        };
        if !contact.entry_hash.starts_with(entry_hash_prefix) {
            return format!(
                "This Confirm is for an older version of `{slug}`'s entry; nothing changed. Use the newest notice or `augmentagent trust confirm {slug}`."
            );
        }
        match self
            .store
            .confirm_trust(&contact.slug, &contact.entry_hash, Self::now())
        {
            Ok(true) => format!("Confirmed: `{slug}` is now a trusted contact."),
            Ok(false) => format!("`{slug}` was already confirmed."),
            Err(e) => format!("Confirm failed: {e}"),
        }
    }
}
