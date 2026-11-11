# Publication recovery

The protected workflow retains the original repository, issue, branch, reviewed
head, request arguments, and create operation before publication. A separate
fenced dispatch reservation records each HTTP attempt and its time. Reservations
survive both a crash before spawning `gh` and a response lost after GitHub accepts
creation; those two outcomes cannot be distinguished from the local intent alone.

Recovery spends no model calls. It scans pending workflows with a durable rotating
cursor, including entries whose retained worktree is currently unavailable. It
first reconciles complete open/closed/merged branch history. A canonical PR URL
returned by `gh` is persisted immediately and subsequently queried directly;
missing list visibility can never authorize another creation once that identity
is known. A changed or closed known publication remains the original identity.

When no identity was returned and complete history is empty, retry admission uses
persisted exponential backoff (one minute through one hour), checks that the issue
is still open and the same repository branch still points at the intended head,
and submits the identical head/base pair. GitHub prevents two simultaneously OPEN
PRs for the same repository head/base pair. This is the server constraint that
makes retrying a not-created transport failure practical; a local operation UUID
or GraphQL clientMutationId is not an idempotency key.

The guarantee is not exactly one HTTP request. Complete history prevents replacing
an observed closed or merged PR. GitHub does not expose an atomic create-once key
covering all terminal history, so concurrent external closure combined with stale
history visibility is a remaining limit of the remote API, not something a local
receipt proves impossible. All attempts and discovered identities remain in the
protected event history. Unknown or incomplete history is a reconciliation
failure, never evidence that creation is safe.

## Manual PR completion and dispatch holds

An authenticated owner can pause a known PR adoption before its first workflow
or command job starts. `autopr pause --workflow OWNER/REPO/pr/NUMBER --json`
records dispatch intent without creating an execution request. Pause remains
separate from active-child teardown and is retained after terminal observation.

After manually merging or closing a registered target, run:

```sh
augmentagent autopr reconcile-target --workflow OWNER/REPO/pr/NUMBER --json
```

This checks current repository authority and the remote target identity/state.
It records an external merge as manual completion or retires a closed target;
no model, build or merge mutation runs and no autonomous allowance is debited.
The normal native-retirement outbox receives terminal owners. Its queued status
is not teardown proof: verify the worker's checked retirement separately.
Open targets, active leases, changed bindings and unresolved side effects stay
held with their original evidence. Never clear a pause, create a dummy command
job or edit SQLite rows to make this maintenance possible.
