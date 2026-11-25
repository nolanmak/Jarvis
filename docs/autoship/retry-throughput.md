# Retry throughput and publish readiness

Autoship keeps one native workflow turn at a time. A paid turn that ends in a
retry yields to eligible peers in the same bounded tick burst (at most five
turns); its actual model charges and fair-scheduling debt are still settled.
Successful turns, genuinely empty selection, merge allowance, physical memory
admission, owner pauses and qualified execution retain their existing gates.

The picker rotates repeated active retry attempts in Build, Verify, Reconcile
and Publish as well as Scope. Human/research and explicit recovery priorities
remain distinct from ordinary due retries. A due durable retry may bypass the
same-day attempt marker; that marker is telemetry, not a day-long retry veto.
This bypass never authorizes a workflow claim, tool, push or merge. Existing
trust, owner, exact candidate, checkout continuity and pending-operation checks
still apply. The worker's thirty-second queue poll also notices due retries,
rather than sleeping until the normal thirty-minute polling interval.

The following classes complete a bounded retry cycle automatically:

| Reason | Action |
| --- | --- |
| `scope_deferred` | `retry_scope` |
| `resource_capacity` | `await_capacity` |
| `verification_infrastructure_retry` | `retry_verification` |
| `publishing_revision` | `push` |
| `acceptance_registration_retry` | `rescope_executable_acceptance` |

Each productive claim records its identity. On normal release or failed guard
drop, the controller records one `retry_turn_finished` event per generation.
Free reconciliation releases do not consume attempts. Dropping a guard only
records scheduling and usage: it does **not** prove descendant teardown,
release its lease, delete native state or authorize another writer.

Unstructured retries back off from five minutes to a one-hour cap. Typed
capacity and verification retry receipts keep their original timestamps and
history. After six consecutive turns in a class without a new contract or
protected evidence, `retry_cycle_exhausted` schedules automatic rescoping using
the preserved request, candidate and witnesses. A changed head or environment
alone does not reset the cycle. Actual source, custody, qualification, spending
and publication checks still run; recovery is scheduling, not evidence.

If a remote operation remains unresolved, the controller records
`retry_cycle_exhausted_pending_effect`, preserves that intent and leaves it to
the existing publication observer with its original eligibility time. Retry
backoff never delays that fenced reconciliation path. It does not invent a successful receipt,
rescope over an unknown remote effect, or blindly dispatch a second push.
Paused work and pending owner input retain owner authority.

Deployment readiness also performs a publish smoke observation: declared alias
identity, the resolved push destination, its exact transport fence, and the
original pre-push admission hook, followed by an actual **Git dry-run** using a
unique probe ref. It never creates or updates that remote ref, grants a workflow
operation, or supplies merge approval. Like ordinary Git dry-runs, it invokes
owner-installed hooks. A refused/timeout smoke is an explicit
`publish-smoke-unverified` deployment failure, not a silent stalled publish.
Runtime alias settings are included in effective configuration matching.

No new environment setting or production state rewrite is required. Normal
worker deployment loads this policy. Existing renamed-origin alias declarations
and the worker's actual Git authentication must still be valid; the smoke uses
the same origin and hooks as publication.

After deployment, inspect retry reasons and distinct productive targets per hour
read-only on the worker host. These are observations, never recovery commands:

```sh
sqlite3 -readonly "$HOME/.local/state/augmentagent/autopr-workflow.sqlite3" \
  "SELECT json_extract(checkpoint,'$.next.reason'), count(*) FROM workflow GROUP BY 1 ORDER BY 2 DESC;"
sqlite3 -readonly "$HOME/.local/state/augmentagent/autopr-workflow.sqlite3" \
  "SELECT strftime('%Y-%m-%dT%H:00Z',at_ms/1000,'unixepoch'),count(*),count(DISTINCT workflow) FROM workflow_event WHERE kind='productive_claimed' GROUP BY 1 ORDER BY 1 DESC LIMIT 24;"
```

Confirm future retry times are bounded, peers rotate, old same-day retries are
selected, and exhausted cycles rescope without new human-only holds. Treat
unknown publication effects as observations to reconcile, not permission to
clear intent. The lane's synthetic tests prove routing and dry-run behavior;
production throughput and actual publication are verified after deployment.
