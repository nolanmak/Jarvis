//! D7 — autonomy needs complete, enabled evidence: the action's newest
//! draft-context receipt exists (so retrieval was enabled and attached
//! before the card), its review finished, it recorded no gaps, and the
//! draft carries no `<!--aa:assumes` block and no needs-input marker.

use augmentagent_store::Store;

/// `Ok(())` when the evidence is complete; `Err(why)` otherwise.
pub fn check(store: &Store, action_id: &str, draft: &str) -> Result<(), String> {
    let receipts = store
        .draft_context_receipts(action_id)
        .map_err(|e| format!("store: {e}"))?;
    let Some(raw) = receipts.last() else {
        return Err(
            "no evidence receipt attached (retrieval disabled or not attached before the card)"
                .into(),
        );
    };
    let receipt: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| format!("unreadable receipt: {e}"))?;
    if receipt.get("review_finished").and_then(|v| v.as_bool()) != Some(true) {
        return Err("the context review did not finish".into());
    }
    match receipt.get("gaps").and_then(|v| v.as_array()) {
        Some(g) if g.is_empty() => {}
        Some(g) => return Err(format!("{} context gap(s)", g.len())),
        None => return Err("receipt has no gaps list".into()),
    }
    if draft.contains("<!--aa:assumes") {
        return Err("the draft assumes facts".into());
    }
    if !augmentagent_approval_discord::split_needs_input(draft)
        .1
        .is_empty()
    {
        return Err("the draft is missing information".into());
    }
    Ok(())
}
