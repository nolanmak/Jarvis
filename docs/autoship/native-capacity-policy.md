# Native capacity policy changes

Filesystem free space and the native retained-state reservation budget are
separate limits. Freeing build caches does not change `max_state_bytes`.
Increasing that budget does not reclaim a session or reduce its reservation.

The Linux owner command below supports the legacy ledger used by current main.
It defaults to preview and requires an explicit target; it selects no budget
automatically. It preserves memory, session count, providers, credentials,
runtime paths, retirement records and retained files. It changes only the
`max_state_bytes` JSON value.

```sh
python3 scripts/native-capacity-policy.py \
  --policy /absolute/private/native-profile/policy.json \
  --max-state-bytes 17179869184
```

The number above is a 16 GiB example, not an approved production setting.
Inspect the returned source/candidate hashes and limits before deciding to
apply. The command emits no provider/auth configuration or native session IDs.
`apply_supported` describes this command's format support, not owner approval,
available physical capacity, successful coding or permission to merge.

Only after owner approval of the concrete proposal:

```sh
python3 scripts/native-capacity-policy.py \
  --policy /absolute/private/native-profile/policy.json \
  --max-state-bytes 17179869184 --apply \
  --expected-source-sha256 REVIEWED_SOURCE_SHA256 \
  --expected-candidate-sha256 REVIEWED_CANDIDATE_SHA256
```

Both hashes are mandatory for apply. A changed source or candidate refuses;
the command does not overwrite an intervening edit. It holds the existing
provision lock, then the existing native state lock, nonblocking, while checking
and publishing. Missing, linked or nonprivate locks are errors; it never creates
replacement coordination files. SQLite is inspected through its read-only VFS,
without opening/copying the live database through a raw file descriptor.
This is a standalone Linux command: the original state lock serializes its
legacy-ledger inspection. WAL/SHM/journal sidecars require recovery and refuse
inspection. The read-only `unix-none` VFS refuses WAL-mode databases without
creating sidecars. Plain SQLite `mode=ro` alone can create WAL/SHM files;
`immutable=1` can ignore their authority, so neither is used as a shortcut.

The private managed policy is published by same-directory atomic rename after
file sync, followed by directory sync and content/path confirmation. Before
rename, failure leaves the original unchanged and removes only this invocation's
temporary file. A retained original file descriptor prevents inode reuse from
turning a replacement temporary path into this invocation's file. A lost rename acknowledgement or failed post-rename sync returns
publication uncertainty; it never rolls back a possibly committed change. Retry
with the same reviewed hashes confirms an already matching target without a
second increase, and must sync it before clearing uncertainty. Confirmation
re-reads the approved bytes and checks the original directory and lock-file
identities after sync; a replaced policy or coordination file cannot produce a
false confirmed result. Before publication, loss of an original lock refuses
without replacing the policy.

This is deliberately not the policy migration protocol for draft #1758.
An authority-epoch marker or unfamiliar native-ledger schema prevents legacy
apply. Absence of a marker alone is insufficient: the private ledger must also
have the exact supported legacy schema. No epoch, binding ancestry or native
permission is manufactured. Damaged/unrecognized state requires recovery.

Current main reads the policy on each new attempt. Existing attempts can retain
their earlier, stricter snapshot. Backoff, model/provider readiness, host resource
admission, owner holds, independent review and required CI remain separate gates.
Do not clear retry/accounting records or restart services to represent a policy
change as successful workflow recovery. No automatic health task applies it.

Exit 0 means preview, confirmed apply or confirmed already-applied contents;
the JSON `reason` distinguishes them. Exit 3 is a guarded refusal. Exit 4 means
unavailable evidence or uncertain publication; inspect `reason` before retry.
`execution_authorized` and `binding_migration_authorized` remain false.

Validation: twenty-one fixture/CLI tests, including lost acknowledgement and sync
failures, plus twenty-three shared-CI contract tests. The native provider bridge
suite runs the new tests. A live **preview only** classified the selected ledger
as legacy and left its 8 GiB policy unchanged. See
[the evidence record](1703-legacy-capacity-policy-proof.json).
