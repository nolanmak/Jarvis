# Durable stage activity (#1706)

The workflow store persists `workflow_activity` in the same transaction as
native workflow checkpoint and verified-gate events. It records the effective
versioned policy, stage start, last meaningful progress, active interval,
inactivity budget and deadline, next observation, typed outcome, and reason.
Owner pause/resume updates the same projection in its control transaction.

`WorkflowStore::activity_status` reads this state without migrating or changing
the database. Missing legacy state is unknown. A heartbeat, usage charge,
head change, or repeated stage metadata cannot establish meaningful progress.
A successful native CI/acceptance gate can establish it; duplicate gate facts
with the same domain and candidate/contract/evidence binding cannot renew it.

The default policy is version 1: scope 15 minutes, build 60 minutes,
deterministic verification 90 minutes, review 30 minutes, and publication/merge
mutation 5 minutes. Original baseline qualification uses the verification
budget even while its workflow stage remains Scope. Waiting for eligibility, capacity, external checks, input,
or an owner resume suspends the active inactivity deadline. Waiting workflows
retain a next observation within five minutes. Heartbeat/lease renewal and
compute accounting remain independent of these clocks.

This implementation milestone provides durable observations. It does not yet
enable inactivity cancellation, recover a running generation, release a lease,
or grant execution/review/merge authority. Those paths must also authenticate
completed guarded-step progress and prove descendant cleanup before lease
release. A long-running process is not proof of progress. Existing command
watchdogs, review requirements, retry backoff, and merge accounting still apply.

Tests use the native workflow store, a real temporary Git repository, injected
UTC millisecond clocks, actual checkpoint/heartbeat/control/gate entry points,
and read-only reopen. Logs for lane validation live under `~/lanes/` rather than
in this repository. No production workflow database is used by these tests.
