//! `augmentagent autonomy explain` — run the gate (and the planner and the
//! candidate lookup) for one stored action, with no side effects: nothing
//! is inserted, sent, approved or changed on a calendar. Pair it with
//! `Store::open_read_only` so even the store file is never written.

use std::fmt::Write as _;

use augmentagent_store::Store;

use crate::gate::{admit, Admission, AutonomyGate, GateInput, ReadyKind, Verdict};

/// Explain the gate's decision for `action_id`. `notices_deliverable`
/// stands in for the live broker's capability.
pub async fn explain(
    gate: &AutonomyGate,
    store: &Store,
    action_id: &str,
    notices_deliverable: bool,
    preview_plan: bool,
) -> anyhow::Result<String> {
    let row = store
        .get_action_with_email(action_id)?
        .ok_or_else(|| anyhow::anyhow!("no action {action_id}"))?;
    let draft = row.action.draft_body.clone().unwrap_or_default();
    let now = chrono::Utc::now().timestamp_millis();
    let input = GateInput {
        action_id,
        email: &row.email,
        draft: &draft,
        now_ms: now,
        notices_deliverable,
    };
    let eval = gate.evaluate(&input).await;
    let mut out = String::new();
    let e = &row.email;
    writeln!(
        out,
        "action:        {action_id} (status {})",
        row.action.status
    )?;
    writeln!(
        out,
        "message:       {} · platform {} · kind {} · thread {}",
        e.message_id,
        e.platform,
        e.kind,
        e.thread_id.as_deref().unwrap_or("-")
    )?;
    writeln!(out, "inbound at:    {}", e.date)?;
    writeln!(
        out,
        "master switch: {}",
        if gate.deps.master_on { "on" } else { "off" }
    )?;
    writeln!(
        out,
        "sender handle: {}",
        eval.trace.sender_handle.as_deref().unwrap_or("-")
    )?;
    write_trace(&mut out, gate, &eval.trace)?;
    match &eval.verdict {
        Verdict::Hold {
            reason,
            contact,
            detail,
        } => {
            let card = if contact.is_some() && reason.notes_card() {
                format!("noted card: \"Not auto-sent: {reason}\"")
            } else if *reason == crate::gate::Reason::Duplicate {
                "no card".into()
            } else {
                "plain card (byte-identical to today's)".into()
            };
            writeln!(out, "decision:      NEEDS APPROVAL — {}", reason.code())?;
            if let Some(d) = detail {
                writeln!(out, "detail:        {d}")?;
            }
            if let Some(c) = contact {
                writeln!(out, "contact:       {} ({} tier)", c.slug, c.tier.as_str())?;
            }
            writeln!(out, "card:          {card}")?;
        }
        Verdict::Ready(ready) => {
            let admission = admit(store, ready, now, true);
            let decision = match &admission {
                Admission::WouldInsert => "AUTO-EXECUTE".to_string(),
                Admission::Hold(r) => format!("NEEDS APPROVAL — {}", r.code()),
                Admission::Inserted { .. } => unreachable!("dry-run admission never inserts"),
            };
            writeln!(out, "decision:      {decision}")?;
            writeln!(
                out,
                "contact:       {} ({} tier)",
                ready.contact.slug,
                ready.contact.tier.as_str()
            )?;
            match &ready.kind {
                ReadyKind::Reply => writeln!(out, "steps:         1. reply on the inbound thread")?,
                ReadyKind::Calendar {
                    action,
                    candidate,
                    event,
                } => {
                    let mut n = 1;
                    if *action == crate::registry::ADD_GUEST {
                        writeln!(
                            out,
                            "steps:         {n}. add {} to \"{}\" on {} (event {})",
                            ready.contact.calendar_email.as_deref().unwrap_or("-"),
                            candidate.title,
                            event.account,
                            event.event_id
                        )?;
                        n += 1;
                    }
                    writeln!(
                        out,
                        "               {n}. text the details on the inbound thread"
                    )?;
                }
            }
            writeln!(out, "would send:    {}", ready.text)?;
            writeln!(out, "idem key:      {}", ready.new_plan.idem_key)?;
        }
    }
    if preview_plan && eval.trace.candidates.is_none() {
        writeln!(
            out,
            "--- plan preview (QA: checks 4–9 skipped; never acted on) ---"
        )?;
        let trace = gate.preview_plan(&row.email, now).await;
        write_trace(&mut out, gate, &trace)?;
    }
    writeln!(out, "side effects:  none (dry run)")?;
    Ok(out)
}

fn write_trace(
    out: &mut String,
    gate: &AutonomyGate,
    trace: &crate::gate::Trace,
) -> std::fmt::Result {
    let eval_trace = trace;
    match &eval_trace.candidates {
        Some(set) => {
            writeln!(
                out,
                "candidates:    {} (window {:?})",
                set.candidates.len(),
                set.window.source
            )?;
            for c in &set.candidates {
                let r = set.resolve(&c.id);
                writeln!(
                    out,
                    "  {} · {} · {} · account {} · organizer {} · join link {} · {} attendee(s){}",
                    c.id,
                    c.title,
                    c.start
                        .with_timezone(&gate.deps.tz)
                        .format("%a %-d %b %-I:%M %p"),
                    c.account,
                    c.is_organizer,
                    if c.join_link.is_some() { "yes" } else { "no" },
                    c.attendees.len(),
                    r.map(|r| format!(" · event {}", r.event_id))
                        .unwrap_or_default(),
                )?;
            }
        }
        None => writeln!(
            out,
            "candidates:    not looked up{}",
            eval_trace
                .candidate_error
                .as_deref()
                .map(|e| format!(" (error: {e})"))
                .unwrap_or_default()
        )?,
    }
    if let Some(raw) = &eval_trace.planner_raw {
        writeln!(out, "planner raw:   {}", raw.trim())?;
    }
    if let Some(plan) = &eval_trace.plan {
        writeln!(out, "plan:          {plan:?}")?;
    }
    Ok(())
}
