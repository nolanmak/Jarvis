# Recovering legacy scope policy holds

The normal worker sweep reconsiders old scope/readiness policy holds without
waiting for model capacity or a successful productive checkout acquisition.
No environment change, readopt command, or one-time operator migration is
required after deploying this change.

Each sweep inspects at most 64 eligible rows in the current repository and
reschedules at most four. Live leases, future retry times, owner pauses,
publication operations and target reconciliation fences remain protected.
Current-policy holds and already reconsidered workflows are left alone.

A `scope_policy_reconsidered` audit event retains the complete old checkpoint.
The current checkpoint moves to `scope_policy_changed` /
`rescope_executable_acceptance`. Binding, evidence, generation, attempts, spend,
PR ownership and original verifier records remain intact. Original verifier
keys stay unchanged where original evidence exists. This transition records
policy reconsideration; it is not a model call, successful verification or merge.

Authenticated legacy `scoper:predicts-refusal` labels are automatically readmitted
through the existing bounded timeline-provenance sweep. Manual label changes,
owner holds, untrusted authors and protected security reasons remain excluded.
New model-only size/path forecasts may publish a valid split; otherwise the
builder continues under the existing actual diff, path, review, CI and resource
limits instead of labelling the issue out based on a forecast.

The policy audit and next checkpoint are committed together. A dry-run or a
read-only health/status query does not run this maintenance.
