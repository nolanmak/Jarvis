# Inactivity recovery lifecycle

S owns the persisted inactivity policy and deadline in `WorkflowStore::activity_status`.
An expired active interval can create a durable `request_idle_recovery` fact. The
request captures the exact workflow, lease generation, stage, binding, deadline,
last meaningful progress and effective policy. Waiting intervals do not expire.
Requests are idempotent and preserve stage, binding, evidence, spend, merge allowance
and lease ownership. Their next action becomes `await_inactivity_teardown`, with the
expired reason visible to progress readers. Pending requests fence native model
admission and productive acquisition; late usage and lease renewal remain recordable.
The original native owner cannot release custody before proof.

`AuditLogger::cancel_owned_runs` requires the logger's exact protected native owner.
It fences subsequent dispatch and signals that owner's registered supervisors through
controller-private cancellation files. Supervisors retain their existing bounded
watchdogs, kill detached descendants and publish cleanup receipts. Other owners are
unaffected. Actual supervised guards settle registrations only after their existing
cleanup checks; a live or unverified guard cannot yield `OwnedRunsQuiescence`.
This proof is a Rust capability, not deserializable model-authored JSON.

`complete_idle_recovery` requires that proof to match the request's live fenced lease
and registered native audit. It preserves the current stage and binding, records
runtime retirement intent, and schedules `await_inactivity_retry` with a retryable
infrastructure outcome for active work. Independent input/CI/quota/capacity/owner holds
and their eligibility remain intact; terminal work is never revived. Lease release is
separate. Repeating completion has no new effect. An unpaid successor that owns no
native/model custody may release its observation lease without clearing the original
request or native owner, so merge observation does not wait on builder teardown.

Unchanged failures back off 5, 10, 20, 40 and then 60 minutes. Heartbeats, lease changes
and new heads do not reset the debt. Trusted meaningful progress after the prior failure,
or a changed stage/policy/contract dependency fingerprint, resets it. Backoff never
consumes merge allowance or imposes a global failed-attempt stop.

The restart custody APIs are described in [restart-custody.md](restart-custody.md).
Native `WorkflowGuard` renewals now consume the persisted policy every 60 seconds.
They commit the request before signalling the owner's supervisors. `finish` consumes
cleanup proof before releasing the lease; dropping an interrupted controller still
never claims teardown or releases custody.

The existing free maintenance path inspects expired pending requests without builder
admission or model calls. Inspection generations cannot dispatch native work or debit
an attempt. Verified successors settle the original request; unknown custody is retained
with a specific blocker and five-minute observation eligibility. Inspection release
preserves owner pause and every independent hold. Productive guards cannot claim this
work ahead of cleanup.

This timer applies to guards with a fenced native owner. Separate supervisor ownership
for other controller roles, broader typed failure producers, legacy writer-custody
recovery and ready-peer dispatch remain part of #1706. These tests do not establish a
production autonomous merge.
