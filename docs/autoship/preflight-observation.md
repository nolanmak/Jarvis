# Pre-workflow readiness observations (#1706)

The automatic worker can fail native execution readiness before it has selected
an issue or created a workflow. A typed `execution-readiness-blocked` exception
now schedules another unpaid check after five minutes (or the configured worker
interval if shorter), instead of waiting for the ordinary thirty-minute tick.

The existing daily-counter file retains `preflight_observation`: schema version,
stable reason code, and next observation time in Unix milliseconds. It is saved
in the same locked, atomic transaction that settles the attempt reservation.
Restart honors the remaining time; a non-preflight result clears the marker.
Read-only preflight exceptions refund historical paid-attempt telemetry. Confirmed
merge accounting remains in WorkflowStore and does not consume this marker.

The observation grants no model/runtime/merge authority and creates no issue
workflow. Each retry still performs the original provider, runtime, resource and
deployment checks. Existing command/recovery wake paths remain available; this
schedule does not delay an authenticated control wake or independent merge sweep.
Unrelated errors retain their existing cadence and telemetry, even if their text
contains `execution-readiness-blocked`. Raw error details are not saved in the
observation record; existing preflight diagnostics retain those details.

Native worker-loop regressions use a fake clock and scripted preflight boundary.
They verify repeated five-minute spacing, no workflow inventory or merge debit,
zero paid-attempt telemetry, remaining time across restart, clearing after recovery,
and rejection of an untyped error with matching text. This is component evidence,
not a live issue-to-merge acceptance run.

Immediate provider-change notification, detailed typed classification of every
preflight refusal, parallel paid builders, and production acceptance remain open.
This change does not close #1706 or assert autoship restoration.
