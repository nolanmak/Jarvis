//! Owner-facing copy for approval outcomes, shared by every approval surface.
//!
//! These functions were private to the Discord event handler (#1190, #1199,
//! #1203). #1289 moved them here unchanged so the Slack surface renders the
//! exact same acknowledgement, resolved-state and recovery text as Discord
//! instead of a copy that could drift. [`card_status_line`] is new: the
//! one-line state a redrawn card shows once its action left `pending`.

use crate::ApprovalActionOutcome;

/// #1190 — did a redraft entry point (Revise/FillAsk modal submit, or the
/// QuickRefine select) fail to repost a new approval card, leaving the owner
/// with no durable signal? True for `Failed`/`NotFound`: `revise` returned no
/// draft, `repost` is `None`, the old card stays, and the ONLY trace is an
/// ephemeral followup a coincidental 🚩 triage notice can visually displace —
/// so those two warrant a durable in-channel notice. False for `Revised` (a
/// fresh card is reposted) and `AlreadyResolved` (the stale card is deleted and
/// the ephemeral explains why), and false for every other outcome, none of
/// which a redraft path can produce. Pure so the decision is exhaustively
/// unit-testable — a new `ApprovalActionOutcome` variant forces a choice here.
pub fn redraft_produced_no_card(outcome: &ApprovalActionOutcome) -> bool {
    matches!(
        outcome,
        ApprovalActionOutcome::Failed { .. } | ApprovalActionOutcome::NotFound
    )
}

/// The acknowledgement shown for an outcome (Discord's ephemeral followup,
/// Slack's ephemeral reply or text-command answer).
pub fn describe(outcome: &ApprovalActionOutcome) -> String {
    match outcome {
        ApprovalActionOutcome::NotFound => {
            "No record of that approval — it may have been cleared.".into()
        }
        ApprovalActionOutcome::AlreadyResolved { status, detail } => {
            resolved_message(status, detail.as_deref())
        }
        ApprovalActionOutcome::Approved => "Approved — sending.".into(),
        ApprovalActionOutcome::Skipped => "Skipped — draft discarded.".into(),
        ApprovalActionOutcome::Revised { .. } => "Revising — new draft posted below.".into(),
        ApprovalActionOutcome::Scheduled { local, .. } => {
            format!("Scheduled — sends {local}.")
        }
        ApprovalActionOutcome::Unscheduled => "Back in the queue — approval card reposted.".into(),
        ApprovalActionOutcome::CancelledSchedule => "Schedule cancelled — draft discarded.".into(),
        ApprovalActionOutcome::Recomposed => {
            "Recomposed — a fresh approval card is posted below. It won't be \
             auto-retired again."
                .into()
        }
        ApprovalActionOutcome::Failed { message } => format!("Failed: {message}"),
    }
}

/// #1203 — should the #1199 recovery ephemeral carry a one-click **Recompose**
/// button for this terminal outcome? Pure so it is exhaustively testable; the
/// button construction stays a thin wrapper over it.
///
/// True only for a `superseded` row whose reason is NOT the empty-draft case
/// (#484: there is literally nothing to recompose — the button would post an
/// empty card). Every other supersede reason (already replied, bulk sender,
/// newer version, `stale`, unknown) is a legitimate owner override: they may
/// still want the drafted reply, so offer the button. The CLI handler defends
/// the empty-draft edge again (a `stale`-reasoned row could in theory carry an
/// empty draft), returning `Failed` rather than carding a blank.
///
/// False for every non-superseded terminal status and for a `None` detail
/// (reason unknown → don't guess a draft exists; the owner can act on the
/// newest card, per the #1199 pointer).
pub fn offers_recompose(status: &str, detail: Option<&str>) -> bool {
    status == "superseded" && detail.is_some_and(|reason| !reason.contains("empty draft body"))
}

/// #1199 — render a terminal `AlreadyResolved { status, detail }` as
/// owner-actionable copy. `detail` is the raw `actions.errorMessage` the store
/// persisted (the specific supersede reason for a `superseded` row; unused for
/// the other terminal statuses).
///
/// Before #1199 every terminal state collapsed to `Already resolved
/// (superseded).`, which never said *why* the card was gone and offered no
/// recovery path. Now the reason is surfaced and each class points at the
/// owner's real next step:
///   - already replied (reconcile Rule 1 / replied-after-scheduling) → the
///     thread is handled, nothing to send;
///   - bulk/automated sender (Rule 2) → no reply was needed;
///   - empty draft body (Rule 3 / #484) → recompose;
///   - newer manual reply / follow-up compose / `stale` / unknown reason →
///     act on the newest card for this thread.
///
/// `detail == None` (row gone, or no stored message) falls through to the
/// "newest card" pointer, which is the safe default for a `superseded` row and
/// never panics.
pub fn resolved_message(status: &str, detail: Option<&str>) -> String {
    match status {
        "superseded" => {
            let reason = detail.unwrap_or_default();
            if reason.contains("already replied")
                || reason.contains("replied on this thread after scheduling")
            {
                "You already handled this thread, so the draft was retired — nothing left to send."
                    .into()
            } else if reason.contains("bulk/automated") {
                "Retired — bulk/automated sender, no reply needed.".into()
            } else if reason.contains("empty draft body") {
                "The draft was empty, so it was cleared — recompose if you meant to reply.".into()
            } else {
                // "superseded by manual reply", "superseded by follow-up
                // compose", "superseded: stale", or any unrecognized reason.
                "A newer version replaced this draft. Act on the newest card for this thread."
                    .into()
            }
        }
        "sent" => "Already sent.".into(),
        "scheduled" => "Already scheduled — see the scheduled notice.".into(),
        "sending" => "Already sending — a send is in flight.".into(),
        "skipped" | "rejected" => "Already skipped — draft discarded.".into(),
        "cancelled" => "Schedule already cancelled.".into(),
        other => format!("Already resolved ({other})."),
    }
}

/// #1289 — the state line a card shows once its action is no longer
/// `pending`, whichever surface (or sweep) moved it. A `superseded` card
/// carries the #1199 reason and recovery pointer; a failed send says the
/// draft did not go out. `None` for `pending`: a pending card shows its
/// controls, not a state line.
pub fn card_status_line(status: &str, detail: Option<&str>) -> Option<String> {
    let line = match status {
        "pending" => return None,
        "sending" => "⏳ Sending…".to_string(),
        "sent" => "✅ Sent.".to_string(),
        "skipped" | "rejected" => "⏭️ Skipped — draft discarded.".to_string(),
        "scheduled" => "🗓️ Scheduled — see the scheduled notice.".to_string(),
        "cancelled" => "🚫 Schedule cancelled — draft discarded.".to_string(),
        "superseded" => format!("♻️ {}", resolved_message(status, detail)),
        "error" => match detail.map(str::trim).filter(|d| !d.is_empty()) {
            Some(d) => format!("⚠️ Not sent: {d}"),
            None => "⚠️ Not sent.".to_string(),
        },
        other => format!("🔒 {}", resolved_message(other, detail)),
    };
    Some(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pending_card_has_no_status_line() {
        assert_eq!(card_status_line("pending", None), None);
    }

    #[test]
    fn every_terminal_state_has_a_line_and_supersede_keeps_its_recovery_pointer() {
        assert_eq!(card_status_line("sent", None).unwrap(), "✅ Sent.");
        assert!(card_status_line("rejected", None)
            .unwrap()
            .contains("Skipped"));
        assert!(card_status_line("sending", None)
            .unwrap()
            .contains("Sending"));
        assert!(card_status_line("scheduled", None)
            .unwrap()
            .contains("Scheduled"));
        let superseded =
            card_status_line("superseded", Some("superseded by manual reply")).unwrap();
        assert!(
            superseded.contains("newest card for this thread"),
            "{superseded}"
        );
        let failed = card_status_line("error", Some("slack send_message: boom")).unwrap();
        assert!(
            failed.contains("Not sent: slack send_message: boom"),
            "{failed}"
        );
        assert_eq!(
            card_status_line("error", Some("  ")).unwrap(),
            "⚠️ Not sent."
        );
        assert!(card_status_line("timed_out", None)
            .unwrap()
            .contains("timed_out"));
    }
}
