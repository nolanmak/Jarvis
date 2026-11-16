# Resolved command target routing (#1706)

After authorization, lifecycle checks, durable PR binding and target resolution,
`targeted::work_once_mode` dispatches a resolved issue through the existing Build
lane and a resolved PR through Resume. An issue that already has a bound PR is
resolved to that PR first; branch names do not select lanes.

This uses the existing lane lock and controller checkout separation. A resume
command therefore does not need the builder lock or builder controller checkout.
Two resume operations still exclude each other. The production dispatcher keeps
execution preflight, checkout identity, native admission, workflow lease,
owner pause, eligibility, resource budgets and exact-head review checks.
AwaitChecks and pending merge reconciliation still take their existing unpaid
paths before this dispatch; they are not turned into builder work.

Tests exercise the production routing handoff with actual flock exclusion: keep
the builder lock held, route both issue-backed and source-less PRs, acquire the
resume lock, and refuse a second resume lock. An issue must still fail to acquire
the busy builder lock, and nested dispatch restores the caller's lane.
These are routing/component tests, not an autonomous-merge or production
concurrency acceptance claim. The serial worker's ready-peer scheduling within
two reconciliation cycles remains separate unfinished #1706 work.
