# Durable stage activity (#1706)

The workflow store persists `workflow_activity` in the same transaction as
native workflow checkpoint and verified-gate events. It records the effective
versioned policy, stage start, last meaningful progress, active interval,
inactivity budget and deadline, next observation, typed outcome, and reason.
Completed original-verifier commands can also establish progress after the executor
validates protected supervisor captures and exact admission/quiescence evidence.
The semantic digest covers the recipe and actual exit/output bytes, excluding
attempt IDs, sessions, capture-manifest IDs and timestamps. A reused historical
command or identical artifact cannot renew activity. Command progress does not
create a verification-gate success or reset unresolved verification backoff.
Read-only capture inspection never records progress.

Owner pause/resume updates the same projection in its control transaction.
Explicit lease release suspends active time; reclaim after release starts a new
active interval. Reclaiming an unreleased crashed generation preserves its old
deadline. Owner resume by itself cannot start an unleased execution interval.

`WorkflowStore::activity_status` reads this state without migrating or changing
the database. Missing legacy state is unknown. A heartbeat, usage charge,
head change, or repeated stage metadata cannot establish meaningful progress.
A successful native CI/acceptance gate can establish it; duplicate gate facts
with the same domain and candidate/contract/evidence binding cannot renew it.

The default policy is version 1: scope 15 minutes, build 60 minutes,
deterministic verification 90 minutes, review 30 minutes, and publication/merge
mutation 5 minutes. Original baseline qualification uses the verification
budget even while its workflow stage remains Scope. Reconcile includes model
acceptance preparation and uses the build/repair budget. A controller holding
the current lease can inject effective thresholds through
`configure_activity_policy`; the change preserves the current interval anchor,
meaningful progress and accounting, and persists across restart. Invalid policy
values and stale leases are rejected transactionally. Waiting for eligibility, capacity, external checks, input,
or an owner resume suspends the active inactivity deadline. Waiting workflows
retain a next observation within five minutes. Heartbeat/lease renewal and
compute accounting remain independent of these clocks.

This implementation milestone provides durable observations. It does not yet
enable inactivity cancellation, recover a running generation, release a lease,
or grant execution/review/merge authority. Those paths must also cover model tools (in addition to original-verifier
commands) and prove descendant cleanup before lease release. A long-running process is not proof of progress. Existing command
watchdogs, review requirements, retry backoff, and merge accounting still apply.

Tests use the native workflow store, a real temporary Git repository, injected
UTC millisecond clocks, actual checkpoint/heartbeat/control/gate entry points,
and read-only reopen. Logs for lane validation live under `~/lanes/` rather than
in this repository. No production workflow database is used by these tests.
