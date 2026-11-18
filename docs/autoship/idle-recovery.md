# Inactivity recovery lifecycle

S owns the persisted inactivity policy and deadline in `WorkflowStore::activity_status`.
An expired active interval can create a durable `request_idle_recovery` fact. The
request captures the exact workflow, lease generation, stage, binding, deadline,
last meaningful progress and effective policy. Waiting intervals do not expire.
Requests are idempotent and do not alter the checkpoint, spend, merge allowance or
lease ownership. Pending requests fence native model admission and dispatch checks;
late usage and lease renewal remain recordable. A pending request prevents release.

`AuditLogger::cancel_owned_runs` requires the logger's exact protected native owner.
It fences subsequent dispatch and signals that owner's registered supervisors through
controller-private cancellation files. Supervisors retain their existing bounded
watchdogs, kill detached descendants and publish cleanup receipts. Other owners are
unaffected. Actual supervised guards settle registrations only after their existing
cleanup checks; a live or unverified guard cannot yield `OwnedRunsQuiescence`.
This proof is a Rust capability, not deserializable model-authored JSON.

`complete_idle_recovery` requires that proof to match the request's live fenced lease
and registered native audit. It preserves the current stage and binding, records
runtime retirement intent, and schedules `await_inactivity_retry` with a retryable-infrastructure outcome. Lease release is
separate and remains forbidden until this completion is committed. Repeating completion
has no new effect.

Unchanged failures back off 5, 10, 20, 40 and then 60 minutes. Heartbeats, lease changes
and new heads do not reset the debt. Trusted meaningful progress after the prior failure,
or a changed stage/policy/contract dependency fingerprint, resets it. Backoff never
consumes merge allowance or imposes a global failed-attempt stop.

This slice provides the lifecycle primitives. It does not enable an automatic expiry
timer. Controller integration, reconstruction of supervised custody after a process
restart, and fenced takeover of an interrupted older-generation request remain part
of #1706. A newly opened logger's empty in-memory registry is not restart teardown
evidence; consumers must not use it to clear an interrupted older lease.
