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

Both hashes are mandatory for apply. They guard reviewed content, not proof of
human approval. Source means SHA-256 of the exact original bytes. Candidate means
SHA-256 of sorted-key, compact JSON plus one newline, replacing only
`max_state_bytes`; duplicate JSON keys refuse. The target must be an integer in
1..2^64−1 and strictly greater than the current budget. A byte-exact already
matching approved candidate with the requested budget is the sole replay exception;
it is synced and confirmed again. Otherwise both current source and computed
candidate must match the supplied hashes, or apply refuses.

It holds the existing
`provision.lock`, then `state/state.lock`, using nonblocking exclusive `flock`, while checking
and publishing. Missing, linked or nonprivate locks are errors; it never creates
replacement coordination files. SQLite is inspected through its read-only VFS,
without opening/copying the live database through a raw file descriptor.
This is a standalone Linux command: the original state lock serializes its
legacy-ledger inspection. WAL/SHM/journal sidecars require recovery and refuse
inspection. The read-only `unix-none` VFS refuses WAL-mode databases without
creating sidecars. Plain SQLite `mode=ro` alone can create WAL/SHM files;
`immutable=1` can ignore their authority, so neither is used as a shortcut.

The profile directory must be absolute and canonical; it and its `state` child
must be private directories owned by the effective UID. Policy and locks must be
regular, single-link, nonsymlink files owned by that UID, with no group/other
permission bits and at most 2 MiB. The ledger has the same restrictions with a
128 MiB bound. The new policy retains permission bits and effective UID ownership;
original group ownership is not promised.

Coordination assumes owner-controlled metadata writers preserve these directory
and lock paths and honor the locks. Provisioning and native state operations use
the same respective locks. This is not an all-writer custody proof against an
uncoordinated process with the same UID; ordinary private permissions do not
exclude such a process. Guest code must not have access to this control plane.

The private managed policy is published by same-directory atomic rename after
file sync, followed by directory sync and content/path confirmation. Before a
rename is attempted, failure leaves the original unchanged. Temporary cleanup
is best effort and checks the retained staging inode before unlinking; a collision
or detected replacement is preserved. The retained descriptor prevents inode
reuse, but check-then-rename/unlink operations are not atomic against an
uncoordinated same-UID writer. Cleanup or descriptor-release errors cannot
overwrite the publication outcome. A lost rename acknowledgement or failed post-rename sync returns
publication uncertainty; it never rolls back a possibly committed change. Retry
with the same reviewed hashes confirms an already matching target without a
second increase, and must sync it before clearing uncertainty. Confirmation
re-reads the approved bytes, requires the observed policy inode to match the held
synced descriptor, and checks original directory and lock-file identities after
sync. Detected replacement reports uncertainty. Loss of an original lock detected
before rename refuses without replacing the policy; a replacement in the final
check/rename interval can publish, then reports uncertainty. This observation is
not a promise that an uncoordinated writer cannot mutate files after confirmation.

This is deliberately not the policy migration protocol for draft #1758.
An authority-epoch marker or unfamiliar native-ledger schema prevents legacy
apply. Absence of a marker alone is insufficient: the private ledger must also
have the exact supported legacy schema: `state/runtime-retirement.sqlite` in
DELETE journal mode, containing only the following table and its implicit primary
key index, with the stored SQL exactly as emitted by current main:

```sql
CREATE TABLE retired_owner(audit_path TEXT NOT NULL,workflow TEXT NOT NULL,generation INTEGER NOT NULL,PRIMARY KEY(audit_path,workflow,generation))
```

Any extra object or altered table definition is unsupported. The marker checked
is `state/authority-epoch.json`. No epoch, binding ancestry or native
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

Validation: twenty-eight fixture/CLI tests, including lost acknowledgement and sync
failures, plus twenty-three shared-CI contract tests. The native provider bridge
suite runs the new tests. A live **preview only** classified the selected ledger
as legacy and left its 8 GiB policy unchanged. See
[the evidence record](1703-legacy-capacity-policy-proof.json).
