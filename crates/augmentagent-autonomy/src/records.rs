//! D11 — idempotency keys for `autonomous_plans`. The tables and their
//! store methods live in `augmentagent_store::autonomy` (re-exported from
//! the crate root).

/// The idempotency window: one plan per thread per 30 minutes, so "not
/// seeing an invite" followed by "??" yields one plan.
pub const IDEM_WINDOW_MS: i64 = 30 * 60 * 1000;

/// `sha256(contact_slug, thread_key, plan_kind, candidate_ref?,
/// floor(created_at / 30 min))`, hex. Fields are length-prefixed so no two
/// different tuples can collide by concatenation.
pub fn idem_key(
    contact_slug: &str,
    thread_key: &str,
    plan_kind: &str,
    candidate_ref: Option<&str>,
    created_at_ms: i64,
) -> String {
    let _ = (contact_slug, thread_key, plan_kind, candidate_ref, created_at_ms);
    todo!()
}
