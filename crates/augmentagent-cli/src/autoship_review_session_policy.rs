//! A fresh-origin receipt authorizes isolation, not reuse of a writer session.
//! Provider names are never substituted for actual conversation custody.
use super::*;
pub(super) fn actor(origin: &ReviewOrigin, actor: &Actor) -> Result<()> {
    if !origin.is_session_isolated() {
        return Ok(());
    }
    let store = crate::autoship_workflow::WorkflowStore::open_read_only(
        &crate::autoship_workflow::default_path(),
    )?
    .context("dependency-adoption-provenance: protected review custody unavailable")?;
    store.require_independent_native_session(&actor.provider, &actor.session_id)
}
pub(super) fn review(
    origin: &ReviewOrigin,
    passes: &[ReviewPass],
    receipt: Option<&AdjudicationReceipt>,
) -> Result<()> {
    if !origin.is_session_isolated() {
        return Ok(());
    }
    let store = crate::autoship_workflow::WorkflowStore::open_read_only(
        &crate::autoship_workflow::default_path(),
    )?
    .context("dependency-adoption-provenance: protected review custody unavailable")?;
    for pass in passes {
        store.require_independent_native_session(&pass.actor.provider, &pass.actor.session_id)?;
        for id in &pass.additional_sessions {
            store.require_independent_native_session(&pass.actor.provider, id)?;
        }
    }
    if let Some(receipt) = receipt {
        store.require_independent_native_session(
            &receipt.actor.provider,
            &receipt.actor.session_id,
        )?;
        for component in &receipt.components {
            store.require_independent_native_session(
                &component.actor.provider,
                &component.actor.session_id,
            )?;
        }
    }
    Ok(())
}
