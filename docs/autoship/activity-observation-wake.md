# Persisted activity observation wake (#1706)

Ordinary registered workflows can suspend their inactivity clock while waiting
for input, quota, capacity or external evidence. S persists their next observation
time. Previously only command jobs and selected recovery lanes could wake the
worker before its normal issue-poll interval, so a failed ordinary tick could
leave an available peer waiting thirty minutes despite a five-minute observation
deadline.

The queue wake path now also reads known due activity. After resolving the
repository, it reserves at most sixteen due waits in one native transaction,
advancing only their next-observation field by five minutes. The ordinary
dispatcher then runs with its existing selection, provider readiness, leases,
owner controls, resource budgets and merge allowance. A reservation is not a
successful dependency check or permission to reclaim the waiting workflow.

Active leases, owner pauses and terminal workflows are excluded. An expired
owner's custody is neither released nor renewed. The scheduling event does not
renew meaningful progress, change the checkpoint, reset a failure fingerprint,
or spend model calls. Re-arming prevents an unchanged wait from becoming a retry
on every thirty-second queue poll. Read-only hints do not migrate legacy state;
missing activity is unknown rather than an invented due workflow or proof of
complete inventory.

Native-store tests cover durable re-arming, read-only reopen, unchanged work and
accounting, expired custody, active/paused exclusion and legacy absence. The two
paused-clock worker tests use actual `run_builds` and the native store, with a
scripted dispatch boundary. They verify a turn within two observation cycles and
five-minute spacing thereafter. They are component tests, not proof that an
ordinary issue has shipped autonomously.

This slice does not add another builder, interrupt productive work, repair an
unavailable repository identity lookup, or cover preflight failures without a
persisted activity record. Provider configuration wakeups, complete failure
producers, broader ready-peer concurrency and production acceptance remain in
#1706.
