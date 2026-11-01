# Recovering a corrupt Autoship adoption cache

`autopr recover-adoption-cache` maintains only the derived `adopted_pr` table.
It does not reset workflow attempts, merge allowance, merge evidence, owner
pauses, messages, or adoption event history. Healthy databases are a no-op.

Inspect first; inspection opens the existing workflow database read-only:

```sh
augmentagent autopr recover-adoption-cache --workflow-db /path/to/autopr-workflow.sqlite3 --json
```

Apply a repair only when inspection reports `repairable-cache-corruption`:

```sh
augmentagent autopr recover-adoption-cache --workflow-db /path/to/autopr-workflow.sqlite3 --apply --json
```

The command refuses unsupported cache schema, dependent triggers or foreign
keys, other corrupt tables, active workflow leases, insufficient storage, or
failure to acquire exclusive SQLite maintenance access. It first rehearses the
repair in memory and verifies every other table, including typed values, row
identities, and SQLite sequence counters. Unsupported layouts or any change to
those fingerprints stop the repair.

Before changing the live cache it captures pinned forensic evidence through
the same managed snapshot implementation as `deploy snapshot --forensic`.
Snapshot budgets, storage reservations, free-space floors, and existing pins
still apply. The forensic image includes committed WAL pages and preserves
damage; it cannot be restored through `deploy restore-db`. It does not take a
healthy rollback snapshot's retention slot. Do not restore an older workflow
database to resolve this incident: that would rewind accounting and controls.

The exclusive maintenance connection stays open through evidence capture,
atomic cache recreation, SQLite `VACUUM`, full integrity checks, and the final
receipt. The source database inode stays in place. The receipt lives at
`adoption-recovery.json` in the pinned snapshot directory and records authority
fingerprints before and after repair. No heuristic row recovery is performed.

If interrupted after publishing the receipt, resume using that same evidence:

```sh
augmentagent autopr recover-adoption-cache --workflow-db /path/to/autopr-workflow.sqlite3 --apply --evidence /path/to/deploy-snapshots/TIMESTAMP-autoship-adoption-recovery --json
```

Resume verifies the database identity, forensic image digest, supported schema,
and unchanged authority. It accepts the original damaged cache or an interrupted
empty rebuild, including interruption after `VACUUM`. A completed receipt is
idempotent while its authority fingerprints remain current. Other changes
require investigation rather than a forced replay.

After repair, normal adoption must rediscover and revalidate PRs against
authenticated GitHub observations. An empty cache does not grant provenance,
permission to merge, or a new allowance. Verify the real workflow's progress
through `autopr status` and a normal admitted `autopr work` attempt.

For forensic capture alone:

```sh
augmentagent --db /path/to/autopr-workflow.sqlite3 deploy snapshot --forensic --label incident
```
